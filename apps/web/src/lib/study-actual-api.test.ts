import { afterEach, expect, it, vi } from "vitest";
import { actualCountsFixture, actualDetailFixture, actualReportFixture, actualSetupFixture, notCalculatedActualResponse, studyActualFixture } from "../../test/study-actual-fixture";
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

it.each([
  { revision: -1 }, { status: "success" }, { studyId: 50 }, { observedThroughUnix: NaN },
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

it("rejects a post-study refit instead of presenting it as the original objective", async () => {
  const actual = actualDetailFixture();
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({ ...actual, report: { ...actual.report, projection: { ...actual.report!.projection, fittedOnPostStudyData: true } } })));
  await expect(getStudyActual(49, new AbortController().signal)).rejects.toBeInstanceOf(StudyActualApiError);
});

it("never guesses objective field spelling or fills absent recipes with today's defaults", () => {
  expect(() => parseOriginalObjective({ version: 1, trend_mode: "blend", trend_bend_days: "linear", trend_blend_lambda: 0.7, forecast: null })).toThrow();
  expect(parseOriginalObjective(actualReportFixture().originalObjective)).toEqual(actualReportFixture().originalObjective);
});

it("requests exactly one revision/job-pinned 50-row page and accepts a real null profile identity", async () => {
  const page = { studyId: 49, jobId: "actual-49-1", revision: 1, retrospective: true, total: 100000, limit: 50, offset: 50, setups: [actualSetupFixture(51)] };
  const fetch = vi.fn().mockResolvedValue(json(page)); vi.stubGlobal("fetch", fetch);
  expect(await getStudyActualSetups(49, 1, "actual-49-1", 50, new AbortController().signal)).toEqual(page);
  expect(fetch.mock.calls[0]?.[0]).toBe("/v1/optimization-studies/49/actual/setups?revision=1&offset=50&limit=50");
  expect(fetch).toHaveBeenCalledOnce();
});

it.each([{ revision: 2 }, { jobId: "other-job" }, { studyId: 50 }])("rejects an artifact-page identity mismatch: %j", async (patch) => {
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({ studyId: 49, jobId: "actual-49-1", revision: 1, retrospective: true, total: 1, limit: 50, offset: 0, setups: [actualSetupFixture()], ...patch })));
  await expect(getStudyActualSetups(49, 1, "actual-49-1", 0, new AbortController().signal)).rejects.toMatchObject({ code: "study_actual_revision_changed" });
});

it("rejects oversized setup pages instead of retaining or quietly truncating them", async () => {
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({ studyId: 49, jobId: "actual-49-1", revision: 1, retrospective: true, total: 1000, limit: 50, offset: 0, setups: Array.from({ length: 51 }, (_, index) => actualSetupFixture(index + 1)) })));
  await expect(getStudyActualSetups(49, 1, "actual-49-1", 0, new AbortController().signal)).rejects.toBeInstanceOf(StudyActualApiError);
});
