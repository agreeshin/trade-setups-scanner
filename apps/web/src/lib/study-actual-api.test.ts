import { afterEach, expect, it, vi } from "vitest";
import { actualCountsFixture, actualDetailFixture, actualReportFixture, actualSetupFixture, havanaActualDetailFixture, notCalculatedActualResponse, originalReportFixture, studyActualFixture } from "../../test/study-actual-fixture";
import { SESSION_EXPIRED_EVENT } from "./platform-api";
import { getStudyActual, getStudyActualSetups, getStudyActualSummary, parseOriginalObjective, parseStudyActualSummary, refreshStudyActual, StudyActualApiError } from "./study-actual-api";

afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });
const json = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status });

it("reads persisted state without a calculating POST and projects away full report/setup payloads", async () => {
  const summary = studyActualFixture();
  const fetch = vi.fn().mockResolvedValue(json({ ...summary, report: { sourceArtifactId: null }, setups: ["not retained"] }));
  vi.stubGlobal("fetch", fetch);
  const signal = new AbortController().signal;
  expect(await getStudyActualSummary(49, signal)).toEqual(summary);
  const [path, init] = fetch.mock.calls[0] as [string, RequestInit];
  expect(path).toBe("/v1/optimization-studies/49/actual");
  expect(init.method).toBeUndefined(); expect(init.body).toBeUndefined(); expect(init.signal).toBe(signal);
  expect(init.credentials).toBe("same-origin"); expect(new Headers(init.headers).get("x-platform-request")).toBe("1");
});

it("submits only an empty manual refresh command and preserves the server's frozen cutoff and job identity", async () => {
  const queued = studyActualFixture(49, { jobId: "job-49-1", revision: 1, status: "queued", stage: "queued", observedThroughUnix: 1795201200 });
  const fetch = vi.fn().mockResolvedValue(json(queued, 202)); vi.stubGlobal("fetch", fetch);
  expect(await refreshStudyActual(49, new AbortController().signal)).toEqual(queued);
  const [path, init] = fetch.mock.calls[0] as [string, RequestInit];
  expect(path).toBe("/v1/optimization-studies/49/actual/refresh"); expect(init.method).toBe("POST"); expect(init.body).toBe("{}");
  expect(new Headers(init.headers).get("content-type")).toBe("application/json");
});

it("accepts the exact backend not_calculated detail envelope with null stage before the first manual refresh", async () => {
  const queued = studyActualFixture(49, { revision: 1, jobId: "first-job", status: "queued", stage: "queued" });
  const fetch = vi.fn().mockResolvedValueOnce(json(notCalculatedActualResponse())).mockResolvedValueOnce(json(queued, 202));
  vi.stubGlobal("fetch", fetch);
  const idle = await getStudyActualSummary(49, new AbortController().signal);
  expect(idle).toEqual(studyActualFixture()); expect(idle.stage).toBeNull(); expect(idle.eligibility.eligible).toBe(true);
  expect(await refreshStudyActual(49, new AbortController().signal)).toEqual(queued);
  expect(fetch).toHaveBeenCalledTimes(2);
});

it("keeps explicit closed/all counts and nullable mean, never deriving either from gates or evaluated rows", () => {
  const actual = studyActualFixture(49, { status: "completed", stage: "completed", counts: { total: 23, closed: 0, tp: 0, sl: 0, gateFailed: 20, unclosed: 3, noData: 0 } });
  expect(parseStudyActualSummary({ ...actual, progress: { evaluatedSetups: 23 }, report: null }, 49)).toEqual(actual);
  expect(parseStudyActualSummary(studyActualFixture(49, { status: "completed", stage: "completed", counts: actualCountsFixture, meanR: 0 }), 49).meanR).toBe(0);
});

it("preserves additive UTC lastSetupAt from the compact summary and actual detail without inferring a legacy value", async () => {
  const lastSetupAt = "2026-11-20T17:45:00.125Z";
  const body = { ...actualDetailFixture(), lastSetupAt };
  const fetch = vi.fn().mockImplementation(() => Promise.resolve(json(body))); vi.stubGlobal("fetch", fetch);
  expect((await getStudyActualSummary(49, new AbortController().signal)).lastSetupAt).toBe(lastSetupAt);
  expect((await getStudyActual(49, new AbortController().signal)).lastSetupAt).toBe(lastSetupAt);
  expect(parseStudyActualSummary({ ...studyActualFixture(), lastSetupAt: null }, 49).lastSetupAt).toBeNull();
  expect(parseStudyActualSummary(studyActualFixture(), 49).lastSetupAt).toBeUndefined();
  expect(fetch).toHaveBeenCalledTimes(2);
});

it.each([
  { revision: -1 }, { lastSetupAt: 1795200000 }, { lastSetupAt: "not-a-timestamp" }, { lastSetupAt: "2026-11-20T17:45:00" }, { status: "success" }, { studyId: 50 }, { observedThroughUnix: NaN },
  { counts: { ...actualCountsFixture, closed: 177 } }, { counts: { ...actualCountsFixture, noData: -1 } },
  { counts: { ...actualCountsFixture, closed: 0 }, meanR: 0 },
  { status: "queued", stage: null }, { status: "running", stage: null },
  { status: "completed", stage: null }, { status: "failed", stage: null }, { stage: undefined },
])("rejects malformed compact actual state: %j", (patch) => {
  expect(() => parseStudyActualSummary({ ...studyActualFixture(), ...patch }, 49)).toThrow(StudyActualApiError);
});

it("exposes the existing job on a same-study busy response without issuing another request", async () => {
  const current = studyActualFixture(49, { revision: 2, jobId: "existing", status: "running", stage: "replaying" });
  const fetch = vi.fn().mockResolvedValue(json({ error: "study_actual_busy", actual: current }, 409)); vi.stubGlobal("fetch", fetch);
  await expect(refreshStudyActual(49, new AbortController().signal)).rejects.toMatchObject({ status: 409, code: "study_actual_busy", current });
  expect(fetch).toHaveBeenCalledTimes(1);
});

it.each([
  [422, "study_actual_ineligible", "execution_start_unavailable", /execution_start_unavailable/],
  [429, "resource_limit", null, /вычислительных ресурсов/],
  [404, "not_found", null, /недоступно или уже удалено/],
  [409, "study_actual_revision_changed", null, /текущую ревизию/],
] as const)("preserves typed rejection %i/%s", async (status, error, reason, message) => {
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({ error, reason }, status)));
  const result = refreshStudyActual(49, new AbortController().signal);
  await expect(result).rejects.toMatchObject({ status, code: error });
  await expect(result).rejects.toThrow(message);
});

it("uses the shared expired-session boundary for unauthorized commands", async () => {
  const listener = vi.fn(); window.addEventListener(SESSION_EXPIRED_EVENT, listener);
  try {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({ error: "unauthorized" }, 401)));
    await expect(refreshStudyActual(49, new AbortController().signal)).rejects.toMatchObject({ status: 401, message: "Сессия истекла. Войдите заново." });
    expect(listener).toHaveBeenCalledOnce();
  } finally { window.removeEventListener(SESSION_EXPIRED_EVENT, listener); }
});

it("reads the real camelCase report shape and keeps server day keys separate from fractional plot instants", async () => {
  const actual = actualDetailFixture();
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({ ...actual, report: { ...actual.report, provenance: { manifest: ["not retained"] } } })));
  expect(await getStudyActual(49, new AbortController().signal)).toEqual(actual);
});

it("accepts factual results with unavailable legacy graph assets and a nullable sourceArtifactId", async () => {
  const report = actualReportFixture({ sourceArtifactId: null, historical: { availability: { status: "unavailable", reason: "original_series_unavailable" }, daily: [] },
    projection: { availability: { status: "unavailable", reason: "original_series_unavailable" }, method: null, points: [], fittedOnPostStudyData: false }, originalObjective: null });
  const actual = actualDetailFixture(49, { report }); vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json(actual)));
  expect(await getStudyActual(49, new AbortController().signal)).toEqual(actual);
});

it("does not expose stale report bytes attached to a queued replacement", async () => {
  const actual = { ...actualDetailFixture(), status: "queued", stage: "queued", counts: null, meanR: null };
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json(actual)));
  expect((await getStudyActual(49, new AbortController().signal)).report).toBeNull();
});

it.each(["not_calculated", "queued", "running", "failed"] as const)("reads independent original report for %s without exposing stale post bytes or starting a job", async (status) => {
  const originalReport = originalReportFixture();
  const body = { ...studyActualFixture(49, { status, stage: status === "not_calculated" ? null : status === "running" ? "replaying" : status }),
    originalReport, report: actualReportFixture() };
  const fetch = vi.fn().mockResolvedValue(json(body)); vi.stubGlobal("fetch", fetch);
  const result = await getStudyActual(49, new AbortController().signal, "UTC");
  expect(result.originalReport).toEqual(originalReport); expect(result.report).toBeNull();
  expect(result.counts).toBeNull(); expect(result.meanR).toBeNull();
  expect(fetch.mock.calls[0]?.[0]).toBe("/v1/optimization-studies/49/actual?timeZone=UTC");
  expect((fetch.mock.calls[0]?.[1] as RequestInit).method).toBeUndefined();
  expect(fetch).toHaveBeenCalledOnce();
});

it("encodes the IANA zone, accepts exact fractional-offset boundaries, and keeps the sealed projection unchanged", async () => {
  const actual = actualDetailFixture(); const timeZone = "Asia/Kathmandu";
  const dayUnix = Date.parse("2026-11-19T18:15:00Z") / 1000;
  const display = { ...actual.report!.display!, timeZone, startDayUnix: dayUnix,
    daily: [{ dayUnix, date: "2026-11-20", sumR: 9, meanR: 0.9, closedCount: 10, tpCount: 7, slCount: 3 }] };
  const report = { ...actual.report!, display };
  const fetch = vi.fn().mockResolvedValue(json({ ...actual, report, originalReport: { ...originalReportFixture(), display: { ...display, daily: [] } } }));
  vi.stubGlobal("fetch", fetch);
  const result = await getStudyActual(49, new AbortController().signal, timeZone);
  expect(fetch.mock.calls[0]?.[0]).toBe("/v1/optimization-studies/49/actual?timeZone=Asia%2FKathmandu");
  expect(result.report!.display!.daily[0]).toMatchObject({ sumR: 9, meanR: 0.9, closedCount: 10 });
  expect(result.report!.projection).toEqual(actual.report!.projection);
});

it("accepts Havana's first repeated midnight and rejects a later PG midnight or mismatched requested zone", async () => {
  const body = havanaActualDetailFixture();
  const fetch = vi.fn().mockImplementation(() => Promise.resolve(json(body))); vi.stubGlobal("fetch", fetch);
  const result = await getStudyActual(49, new AbortController().signal, "America/Havana");
  expect(result.report!.display!.startDayUnix).toBe(Date.parse("2026-11-01T04:00:00Z") / 1000);
  expect(result.report!.display!.daily.at(-1)).toMatchObject({ date: "2026-11-01", meanR: 0.2, closedCount: 5 });
  expect(result.report!.actual.daily[0]!.plotAtUnix).toBe(Date.parse("2026-11-01T04:30:00Z") / 1000);
  expect(fetch.mock.calls[0]?.[0]).toBe("/v1/optimization-studies/49/actual?timeZone=America%2FHavana");
  await expect(getStudyActual(49, new AbortController().signal, "America/New_York")).rejects.toBeInstanceOf(StudyActualApiError);
  const laterMidnight = Date.parse("2026-11-01T05:00:00Z") / 1000;
  const report = { ...body.report!, executionStartedAt: "2026-11-01T05:30:00.125Z",
    display: { ...body.report!.display!, startDayUnix: laterMidnight,
      daily: body.report!.display!.daily.map((point) => point.date === "2026-11-01" ? { ...point, dayUnix: laterMidnight } : point) } };
  fetch.mockImplementation(() => Promise.resolve(json({ ...body, report })));
  await expect(getStudyActual(49, new AbortController().signal, "America/Havana")).rejects.toBeInstanceOf(StudyActualApiError);
});

it.each([
  { timeZone: "Not/AZone" }, { timeZone: "America/New_York" }, { startDayUnix: 1795176000 },
  { daily: [{ dayUnix: 1795132800, date: "2026-11-21", sumR: 1, meanR: 1, closedCount: 1, tpCount: 1, slCount: 0 }] },
  { daily: [{ dayUnix: 1795132800, date: "2026-11-20", sumR: 1, meanR: 0, closedCount: 0, tpCount: 0, slCount: 0 }] },
  { daily: [{ dayUnix: 1795132800, date: "2026-11-20", sumR: 9, meanR: 0.5, closedCount: 10, tpCount: 7, slCount: 3 }] },
  { daily: [{ dayUnix: 1795132800, date: "2026-11-20", sumR: 1, meanR: 1, closedCount: 1, tpCount: 0, slCount: 0 }] },
])("rejects malformed or wrongly grouped display metadata instead of relabeling UTC aggregates: %j", async (patch) => {
  const actual = actualDetailFixture();
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({ ...actual, report: { ...actual.report, display: { ...actual.report!.display, ...patch } } })));
  await expect(getStudyActual(49, new AbortController().signal, "UTC")).rejects.toBeInstanceOf(StudyActualApiError);
});

it("rejects duplicate day points, missing display and oversized display collections on the zone-aware path", async () => {
  const actual = actualDetailFixture(); const point = actual.report!.display!.daily[0]!;
  for (const display of [undefined, { ...actual.report!.display, daily: [point, point] }, { ...actual.report!.display, daily: Array.from({ length: 3662 }, () => point) }]) {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({ ...actual, report: { ...actual.report, display } })));
    await expect(getStudyActual(49, new AbortController().signal, "UTC")).rejects.toBeInstanceOf(StudyActualApiError);
  }
});

it("keeps truthful original-unavailable provenance separate from an available raw replay", async () => {
  const originalReport = originalReportFixture();
  originalReport.display = { ...originalReport.display!, daily: [], historical: { availability: { status: "unavailable", reason: "original_outcomes_unavailable" }, fidelity: "unavailable", complete: false,
    sourceIdentity: { runId: 36, rawSetupCount: 1767, rawReplayAvailable: true } } };
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({ ...notCalculatedActualResponse(36), originalReport })));
  const actual = await getStudyActual(36, new AbortController().signal, "UTC");
  expect(actual.originalReport!.display!.historical.availability.status).toBe("unavailable");
  expect(actual.originalReport!.display!.historical.sourceIdentity).toEqual({ runId: 36, rawSetupCount: 1767, rawReplayAvailable: true });
  expect(actual.originalReport!.display!.daily).toEqual([]);
});

it("rejects a post-study refit instead of presenting it as the original objective", async () => {
  const actual = actualDetailFixture();
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({ ...actual, report: { ...actual.report, projection: { ...actual.report!.projection, fittedOnPostStudyData: true } } })));
  await expect(getStudyActual(49, new AbortController().signal)).rejects.toBeInstanceOf(StudyActualApiError);
});

function combinedDetail() {
  const detail = actualDetailFixture();
  return { ...detail, report: { ...detail.report!, combinedProjection: {
    availability: { status: "available" as const }, method: "decayed_probability", fittedOnPostStudyData: true as const,
    points: [{ atUnix: detail.report!.projection.points[0]!.atUnix, value: -0.25 }, { atUnix: detail.report!.observedThroughUnix, value: 0.45 }],
    observationCutoff: detail.report!.observedThroughUnix, revision: detail.revision, jobId: detail.jobId,
  } } };
}

it("reads the additive combined projection without replacing or deriving the sealed original", async () => {
  const body = combinedDetail();
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json(body)));
  const detail = await getStudyActual(49, new AbortController().signal, "UTC");
  expect(detail.report!.combinedProjection).toEqual(body.report.combinedProjection);
  expect(detail.report!.projection).toEqual(body.report.projection);
  expect(detail.report!.originalObjective).toEqual(body.report.originalObjective);
});

it.each([{ revision: 2 }, { jobId: "different-job" }])("hides stale combined identity %j but retains the original projection", async (patch) => {
  const body = combinedDetail();
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({ ...body, report: { ...body.report, combinedProjection: { ...body.report.combinedProjection, ...patch } } })));
  const detail = await getStudyActual(49, new AbortController().signal, "UTC");
  expect(detail.report!.combinedProjection!.availability).toEqual({ status: "unavailable", reason: "post_study_stale" });
  expect(detail.report!.combinedProjection!.points).toEqual([]);
  expect(detail.report!.projection).toEqual(body.report.projection);
});

it("hides a previous-cutoff combined model without suppressing the original line", async () => {
  const body = combinedDetail();
  const oldCutoff = body.report.observedThroughUnix - 60;
  const combinedProjection = { ...body.report.combinedProjection, observationCutoff: oldCutoff,
    points: body.report.combinedProjection.points.map((point) => ({ ...point, atUnix: Math.min(point.atUnix, oldCutoff) })) };
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({ ...body, report: { ...body.report, combinedProjection } })));
  const detail = await getStudyActual(49, new AbortController().signal, "UTC");
  expect(detail.report!.combinedProjection!.availability).toEqual({ status: "unavailable", reason: "post_study_stale" });
  expect(detail.report!.projection).toEqual(body.report.projection);
});

it.each([
  { fittedOnPostStudyData: false }, { revision: 0 }, { observationCutoff: 1 }, { points: [{ atUnix: 0, value: "0.2" }] },
  { points: Array.from({ length: 3663 }, () => ({ atUnix: 0, value: 1 })) },
  { points: [{ atUnix: 2, value: 1 }, { atUnix: 1, value: 1 }] },
])("rejects malformed or oversized diagnostic projection %j", async (patch) => {
  const body = combinedDetail();
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({ ...body, report: { ...body.report, combinedProjection: { ...body.report.combinedProjection, ...patch } } })));
  await expect(getStudyActual(49, new AbortController().signal, "UTC")).rejects.toBeInstanceOf(StudyActualApiError);
});

it.each(["queued", "running", "failed"] as const)("discards combined model attached to a %s actual replacement and preserves original-only report", async (status) => {
  const body = combinedDetail();
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({ ...body, status, stage: status === "running" ? "evaluating" : status })));
  const detail = await getStudyActual(49, new AbortController().signal, "UTC");
  expect(detail.report).toBeNull();
  expect(detail.originalReport!.projection).toEqual(body.originalReport!.projection);
});

it("never guesses objective field spelling or fills absent recipes with today's defaults", () => {
  expect(() => parseOriginalObjective({ version: 1, trend_mode: "blend", trend_bend_days: "linear", trend_blend_lambda: 0.7, forecast: null })).toThrow();
  expect(parseOriginalObjective(actualReportFixture().originalObjective)).toEqual(actualReportFixture().originalObjective);
});

it("requests exactly one revision/job-pinned 50-row page and accepts a real null profile identity", async () => {
  const page = { studyId: 49, jobId: "actual-49-1", revision: 1, retrospective: true, total: 100000, limit: 50, offset: 50, setups: [actualSetupFixture(51)] };
  const fetch = vi.fn().mockResolvedValue(json(page)); vi.stubGlobal("fetch", fetch);
  expect(await getStudyActualSetups(49, 1, "actual-49-1", 50, new AbortController().signal)).toEqual(page);
  expect(fetch.mock.calls[0]?.[0]).toBe("/v1/optimization-studies/49/actual/setups?revision=1&offset=50&limit=50&hideGateFailed=true");
  expect((fetch.mock.calls[0]?.[1] as RequestInit).method).toBeUndefined();
  expect((fetch.mock.calls[0]?.[1] as RequestInit).body).toBeUndefined();
  expect(fetch).toHaveBeenCalledOnce();
});

it("explicitly sends false to include gate failures without a write", async () => {
  const page = { studyId: 49, jobId: "actual-49-1", revision: 1, retrospective: true, total: 176, limit: 50, offset: 0,
    setups: [{ ...actualSetupFixture(), status: "gate_failed", riskReturn: null }] };
  const fetch = vi.fn().mockResolvedValue(json(page)); vi.stubGlobal("fetch", fetch);
  expect(await getStudyActualSetups(49, 1, "actual-49-1", 0, new AbortController().signal, false)).toEqual(page);
  expect(fetch.mock.calls[0]?.[0]).toBe("/v1/optimization-studies/49/actual/setups?revision=1&offset=0&limit=50&hideGateFailed=false");
  expect((fetch.mock.calls[0]?.[1] as RequestInit).method).toBeUndefined();
  expect((fetch.mock.calls[0]?.[1] as RequestInit).body).toBeUndefined();
});

it.each([{ revision: 2 }, { jobId: "other-job" }, { studyId: 50 }])("rejects an artifact-page identity mismatch: %j", async (patch) => {
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({ studyId: 49, jobId: "actual-49-1", revision: 1, retrospective: true, total: 1, limit: 50, offset: 0, setups: [actualSetupFixture()], ...patch })));
  await expect(getStudyActualSetups(49, 1, "actual-49-1", 0, new AbortController().signal)).rejects.toMatchObject({ code: "study_actual_revision_changed" });
});

it("rejects oversized setup pages instead of retaining or quietly truncating them", async () => {
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({ studyId: 49, jobId: "actual-49-1", revision: 1, retrospective: true, total: 1000, limit: 50, offset: 0, setups: Array.from({ length: 51 }, (_, index) => actualSetupFixture(index + 1)) })));
  await expect(getStudyActualSetups(49, 1, "actual-49-1", 0, new AbortController().signal)).rejects.toBeInstanceOf(StudyActualApiError);
});
