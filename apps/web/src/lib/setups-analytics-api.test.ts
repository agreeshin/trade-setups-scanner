import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { SetupsAnalyticsApiError, evaluateIssuedStudyExpectancy, fetchExpectancyPngUrl, fetchIssuedStudyExpectancy, fetchSetupsAnalytics, fetchSetupsAnalyticsStudies, parseIssuedStudyExpectancy, type IssuedStudyExpectancy } from "./setups-analytics-api";

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });
const studyMeta = { studyId: 40, externalName: "Всплеск цены и оборота", setupCount: 12, generationId: 36, artifactId: 900 };
const setup = {
  id: 9, symbol: "BTCUSDT", exchange: "bybit", slotId: 1, studyId: 40, direction: "short",
  setupTimeUnix: 100, entryTimeUnix: 400, entryPrice: 98.5, gateThreshold: 98, tpPrice: 78.8,
  slPrice: 104.41, closeTimeUnix: 900, riskReturn: -1, status: "sl",
};
const result = {
  totals: { selected: 5, gatePassed: 3, closed: 2, meanR: 1.25 },
  studies: [{ studyId: 40, selected: 5, gatePassed: 3, closed: 2, meanR: 1.25 }],
  setups: [setup],
};

const calls: string[] = [];
const methods: string[] = [];
beforeEach(() => {
  calls.length = 0;
  methods.length = 0;
  Object.defineProperty(URL, "createObjectURL", { configurable: true, writable: true, value: vi.fn(() => "blob:expectancy-png") });
  Object.defineProperty(URL, "revokeObjectURL", { configurable: true, writable: true, value: vi.fn() });
  vi.stubGlobal("fetch", vi.fn((path: string) => {
    calls.push(path);
    return Promise.resolve(json({ studies: [studyMeta] }));
  }));
});
afterEach(() => { vi.unstubAllGlobals(); vi.restoreAllMocks(); });

it("loads releaser studies for the algorithm and range", async () => {
  const items = await fetchSetupsAnalyticsStudies("price_volume_spike", 1, 2);
  expect(items).toHaveLength(1);
  expect(items[0]!.studyId).toBe(40);
  expect(items[0]!.artifactId).toBe(900);
  expect(calls[0]).toContain("/v1/setups/analytics/studies?algorithmId=price_volume_spike&fromUnix=1&toUnix=2");
});

it("posts selected studies in the unchanged query and parses the full result including exit price", async () => {
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    calls.push(path);
    methods.push(init?.method ?? "GET");
    return Promise.resolve(json({ ...result, setups: [{ ...setup, closePrice: 104.41 }] }));
  }));
  const data = await fetchSetupsAnalytics("a", 1, 2, [40, 41]);
  expect(data.totals.meanR).toBe(1.25);
  expect(data.studies[0]!.studyId).toBe(40);
  expect(data.setups[0]!.status).toBe("sl");
  expect(data.setups[0]!.closePrice).toBe(104.41);
  expect(methods).toEqual(["POST"]);
  expect(new URL(calls[0]!, "http://localhost").searchParams.get("studies")).toBe("40,41");
});

it("does not fall back to a calculating GET when POST is unsupported", async () => {
  const fetch = vi.fn((_path: string, init?: RequestInit) => { methods.push(init?.method ?? "GET"); return Promise.resolve(json({}, 405)); });
  vi.stubGlobal("fetch", fetch);
  await expect(fetchSetupsAnalytics("a", 1, 2, null)).rejects.toThrow("HTTP 405");
  expect(methods).toEqual(["POST"]); expect(fetch).toHaveBeenCalledTimes(1);
});

it("omits studies when all are selected", async () => {
  vi.stubGlobal("fetch", vi.fn((path: string) => {
    calls.push(path);
    return Promise.resolve(json(result));
  }));
  await fetchSetupsAnalytics("a", 1, 2, null);
  expect(new URL(calls[0]!, "http://localhost").searchParams.has("studies")).toBe(false);
});

it("loads the artifact png as an object url", async () => {
  vi.stubGlobal("fetch", vi.fn((path: string) => {
    calls.push(path);
    return Promise.resolve(new Response(new Blob(["png-bytes"]), { status: 200 }));
  }));
  const url = await fetchExpectancyPngUrl(900);
  expect(url).toBe("blob:expectancy-png");
  expect(calls[0]).toContain("/v1/setups/analytics/expectancy-png/900");
});

it("reports png http failures", async () => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ error: "expectancy_png_unavailable" }, 404))));
  await expect(fetchExpectancyPngUrl(900)).rejects.toThrow("HTTP 404");
});

it("maps the limit error to a range hint", async () => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ error: "setups_analytics_limit_exceeded" }, 400))));
  const cause = await fetchSetupsAnalytics("a", 1, 2, null).catch((error: unknown) => error);
  expect(cause).toBeInstanceOf(SetupsAnalyticsApiError);
  expect((cause as SetupsAnalyticsApiError).message).toContain("Сузьте диапазон");
});

it("reports other HTTP failures with the status", async () => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ error: "boom" }, 500))));
  await expect(fetchSetupsAnalytics("a", 1, 2, null)).rejects.toThrow("HTTP 500");
  await expect(fetchSetupsAnalyticsStudies("a", 1, 2)).rejects.toThrow("HTTP 500");
});

it("rejects malformed payloads", async () => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ totals: { selected: -1 } }))));
  await expect(fetchSetupsAnalytics("a", 1, 2, null)).rejects.toThrow(SetupsAnalyticsApiError);
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ studies: [{ studyId: "x" }] }))));
  await expect(fetchSetupsAnalyticsStudies("a", 1, 2)).rejects.toThrow(/исследований|studies/);
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ setups: [{ id: 1, status: "unknown" }] }))));
  await expect(fetchSetupsAnalytics("a", 1, 2, null)).rejects.toThrow(SetupsAnalyticsApiError);
});

it("retains compact saved scalar metrics and a valid calculated-cohort snapshot without matrix/detail reads", async () => {
  const meta = { ...studyMeta, bestScore: 0.99, forecastBest: { tpPct: 30, slPct: 2, forecastR: 1.9, meanR: 0.1 }, bestCell: { tpPct: 14, slPct: 4, rawR: -0.3 } };
  const snapshot = { snapshotId: 9, observationCutoff: 1200, fingerprint: "a".repeat(64) };
  vi.stubGlobal("fetch", vi.fn((path: string) => Promise.resolve(json(path.includes("/studies?") ? { studies: [meta] } : { ...result, snapshot }))));
  expect((await fetchSetupsAnalyticsStudies("a", 1, 2))[0]).toEqual(meta);
  expect((await fetchSetupsAnalytics("a", 1, 2, null)).snapshot).toEqual(snapshot);
  expect(vi.mocked(fetch)).toHaveBeenCalledTimes(2);
});

it("keeps unavailable saved metrics nullable and rejects malformed additive fields rather than guessing defaults", async () => {
  const nullable = { ...studyMeta, bestScore: null, forecastBest: null, bestCell: null };
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ studies: [nullable] }))));
  expect((await fetchSetupsAnalyticsStudies("a", 1, 2))[0]).toEqual(nullable);
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ studies: [{ ...studyMeta, bestCell: { tpPct: 14, slPct: 4 } }] }))));
  await expect(fetchSetupsAnalyticsStudies("a", 1, 2)).rejects.toThrow(SetupsAnalyticsApiError);
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ ...result, snapshot: { snapshotId: 9, observationCutoff: 1201, fingerprint: "bad" } }))));
  await expect(fetchSetupsAnalytics("a", 1, 2, null)).rejects.toThrow(SetupsAnalyticsApiError);
});

function projectionFixture(evaluated = false): IssuedStudyExpectancy {
  const day = { dayUnix: 86400, tpCount: 2, slCount: 1, closedCount: 3, historicalClosedCount: 3, issuedClosedCount: 0, sumR: 3, meanR: 1 };
  return { version: 1, studyId: 40, sourceKind: "persisted_issued_setups", retrospective: false,
    objective: { version: 1, trendMode: "blend", trendBendDays: "14", trendBlendLambda: 0.35,
      forecast: { method: "decayed_probability", version: 1, halfLifeDays: 7, kappa: 3, priorPolicy: "gross_break_even" } },
    recommendedPair: { tpPct: 10, slPct: 5 },
    originalSource: { studyIdentity: "saved-study-40", artifactId: 900, artifactKey: "original-900", revisionKey: "original", observationCutoff: 172800 },
    issuedSource: { availability: { status: "available" }, outcomePolicy: "issued_open_entry_sl_first_10080m", setupCount: 2, closedCount: 2,
      scientificDay: "raw_signal_close_utc", observedDay: "entry_utc" },
    filter: { fromUnix: null, toUnix: null, q: null }, snapshot: { snapshotId: 55, observationCutoff: 259200, fingerprint: "b".repeat(64) }, startDayUnix: 172800,
    originalDaily: [day], daily: [day, { ...day, dayUnix: 172800, tpCount: 1, slCount: 1, closedCount: 2, historicalClosedCount: 0, issuedClosedCount: 2, sumR: 1, meanR: 0.5 }],
    originalProjection: { availability: { status: "available" }, method: "decayed_probability", meanR: 1, forecastR: 0.7, decisionR: 0.895,
      fittedOnIssuedData: false, points: [{ atUnix: 86400, value: 0.4 }, { atUnix: 172800, value: 0.895 }] },
    combinedProjection: evaluated ? { availability: { status: "available" }, method: "decayed_probability", meanR: 0.8, forecastR: 0.6, decisionR: 0.73,
      fittedOnIssuedData: true, points: [{ atUnix: 86400, value: 0.3 }, { atUnix: 259200, value: 0.73 }] }
      : { availability: { status: "unavailable", reason: "issued_projection_not_evaluated" }, method: null, meanR: null, forecastR: null, decisionR: null, fittedOnIssuedData: true, points: [] } };
}

it("reads the direct issued DTO without evaluating on GET, then POSTs only the unchanged server snapshot and empty full-study filter", async () => {
  const controller = new AbortController();
  vi.stubGlobal("fetch", vi.fn((_path: string, init?: RequestInit) => Promise.resolve(json(projectionFixture(init?.method === "POST")))));
  const saved = await fetchIssuedStudyExpectancy(40, {}, controller.signal);
  expect(saved.combinedProjection.availability).toEqual({ status: "unavailable", reason: "issued_projection_not_evaluated" });
  expect(fetch).toHaveBeenCalledTimes(1);
  expect(vi.mocked(fetch).mock.calls[0]).toEqual(["/v1/setups/analytics/study-expectancy/40/projection", expect.objectContaining({ signal: controller.signal })]);
  expect(vi.mocked(fetch).mock.calls[0]![1]?.body).toBeUndefined();
  const evaluated = await evaluateIssuedStudyExpectancy(saved, controller.signal);
  expect(evaluated.combinedProjection.decisionR).toBe(0.73);
  expect(evaluated.originalProjection).toEqual(saved.originalProjection);
  expect(evaluated.snapshot).toEqual(saved.snapshot);
  expect(vi.mocked(fetch).mock.calls[1]![0]).toBe("/v1/setups/analytics/study-expectancy/40/projection");
  expect(JSON.parse(vi.mocked(fetch).mock.calls[1]![1]!.body as string)).toEqual({ filter: {}, snapshot: saved.snapshot });
  expect(vi.mocked(fetch).mock.calls[1]![1]).toMatchObject({ method: "POST", signal: controller.signal });
});

it("preserves the exact optional bounded filter between read and evaluation, never sending live/daily or timezone fields", async () => {
  const filter = { fromUnix: 10, toUnix: 99, q: "%_" };
  vi.stubGlobal("fetch", vi.fn((_path: string, init?: RequestInit) => Promise.resolve(json({ ...projectionFixture(init?.method === "POST"), filter }))));
  const saved = await fetchIssuedStudyExpectancy(40, filter, new AbortController().signal);
  await evaluateIssuedStudyExpectancy(saved, new AbortController().signal);
  const calls = vi.mocked(fetch).mock.calls;
  const path = calls[0]![0];
  if (typeof path !== "string") throw new Error("Expected a string URL");
  const query = new URL(path, "http://fixture").searchParams;
  expect(Object.fromEntries(query)).toEqual({ fromUnix: "10", toUnix: "99", q: "%_" });
  expect(JSON.parse(calls[1]![1]!.body as string)).toEqual({ filter, snapshot: saved.snapshot });
});

it("reports a changed fingerprint 409 once, without refreshing saved outcomes, retrying GET or using the legacy live fit", async () => {
  const saved = projectionFixture();
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ error: "setups_study_projection_snapshot_changed" }, 409))));
  const cause: unknown = await evaluateIssuedStudyExpectancy(saved, new AbortController().signal).catch((cause: unknown) => cause);
  expect(cause).toBeInstanceOf(SetupsAnalyticsApiError);
  if (!(cause instanceof SetupsAnalyticsApiError)) throw new Error("Expected API error");
  expect(cause.status).toBe(409); expect(cause.message).toContain("Снимок выпущенных сетапов изменился");
  expect(fetch).toHaveBeenCalledTimes(1);
  expect(vi.mocked(fetch).mock.calls[0]![0]).toBe("/v1/setups/analytics/study-expectancy/40/projection");
});

it.each([
  ["fingerprint", (dto: IssuedStudyExpectancy) => { dto.snapshot.fingerprint = "c".repeat(64); }],
  ["cutoff", (dto: IssuedStudyExpectancy) => { dto.snapshot.observationCutoff += 60; }],
  ["snapshotId", (dto: IssuedStudyExpectancy) => { dto.snapshot.snapshotId += 1; }],
  ["study source", (dto: IssuedStudyExpectancy) => { dto.originalSource.studyIdentity = "other"; }],
  ["recipe", (dto: IssuedStudyExpectancy) => { dto.objective.trendBlendLambda = 0.7; }],
  ["pair", (dto: IssuedStudyExpectancy) => { dto.recommendedPair.tpPct = 20; }],
  ["sealed curve", (dto: IssuedStudyExpectancy) => { dto.originalProjection.points[0]!.value = 99; }],
  ["issued counts", (dto: IssuedStudyExpectancy) => { dto.issuedSource.setupCount += 1; }],
] as const)("rejects evaluated responses whose %s changes even if HTTP succeeded", async (_name, change) => {
  const evaluated = projectionFixture(true); change(evaluated);
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json(evaluated))));
  await expect(evaluateIssuedStudyExpectancy(projectionFixture(), new AbortController().signal)).rejects.toThrow(SetupsAnalyticsApiError);
});

it.each([
  ["legacy wrapper", (dto: IssuedStudyExpectancy): unknown => ({ issuedStudy: dto })],
  ["legacy fit", (): unknown => ({ studyId: 40, meanR: 1, daily: [], trend: { method: "mean", points: [] } })],
  ["wrong study", (dto: IssuedStudyExpectancy) => ({ ...dto, studyId: 41 })],
  ["retrospective alias", (dto: IssuedStudyExpectancy) => ({ ...dto, retrospective: true })],
  ["source alias", (dto: IssuedStudyExpectancy) => ({ ...dto, sourceKind: "post_study_replay" })],
  ["unknown recipe version", (dto: IssuedStudyExpectancy) => ({ ...dto, objective: { ...dto.objective, version: 2 } })],
  ["negative counts", (dto: IssuedStudyExpectancy) => ({ ...dto, daily: [{ ...dto.daily[0], tpCount: -1 }] })],
  ["average of means", (dto: IssuedStudyExpectancy) => ({ ...dto, daily: [{ ...dto.daily[0], meanR: 0.5 }] })],
  ["invented timezone bucket", (dto: IssuedStudyExpectancy) => ({ ...dto, daily: [{ ...dto.daily[0], dayUnix: 86400 + 3600 }] })],
  ["future curve", (dto: IssuedStudyExpectancy) => ({ ...dto, originalProjection: { ...dto.originalProjection, points: [{ atUnix: dto.snapshot.observationCutoff + 1, value: 1 }] } })],
  ["unavailable with fake zero", (dto: IssuedStudyExpectancy) => ({ ...dto, combinedProjection: { ...dto.combinedProjection, decisionR: 0 } })],
  ["wrong fit ownership", (dto: IssuedStudyExpectancy) => ({ ...dto, originalProjection: { ...dto.originalProjection, fittedOnIssuedData: true } })],
  ["oversized curve", (dto: IssuedStudyExpectancy) => ({ ...dto, originalProjection: { ...dto.originalProjection, points: Array.from({ length: 10001 }, () => ({ atUnix: 86400, value: 1 })) } })],
] as const)("rejects %s instead of guessing or coercing into a retrospective report", (_name, malformed) => {
  expect(() => parseIssuedStudyExpectancy(malformed(projectionFixture()), 40)).toThrow(SetupsAnalyticsApiError);
});

it("rejects a calculating GET response rather than treating implicit combined evaluation as the read contract", async () => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json(projectionFixture(true)))));
  await expect(fetchIssuedStudyExpectancy(40, {}, new AbortController().signal)).rejects.toThrow(SetupsAnalyticsApiError);
  expect(fetch).toHaveBeenCalledTimes(1);
});

it("keeps explicit unavailable original/issued sources and null curve gaps without inventing metrics", () => {
  const dto = projectionFixture();
  dto.originalSource.artifactId = null; dto.originalSource.artifactKey = null; dto.originalSource.observationCutoff = null;
  dto.originalProjection = { ...dto.combinedProjection, availability: { status: "unavailable", reason: "original_model_unavailable" }, fittedOnIssuedData: false };
  dto.issuedSource = { ...dto.issuedSource, availability: { status: "unavailable", reason: "issued_outcomes_missing" }, closedCount: null };
  expect(parseIssuedStudyExpectancy(dto, 40).originalProjection.decisionR).toBeNull();
  const available = projectionFixture();
  available.originalProjection.points.splice(1, 0, { atUnix: 129600, value: null });
  expect(parseIssuedStudyExpectancy(available, 40).originalProjection.points[1]!.value).toBeNull();
});
