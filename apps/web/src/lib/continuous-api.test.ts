import { afterEach, expect, it, vi } from "vitest";
import { getContinuousStudy, listContinuousStudies, saveContinuousStudy, startContinuousStudy, stopContinuousStudy, type ContinuousConfig, type ContinuousProcess } from "./continuous-api";
import { cancelOptimizationStudy, deleteOptimizationStudy, getOptimizationStudy, listOptimizationStudies, summarizeOptimizationStudy, type OptimizationStudy } from "./optimization-api";

const config: ContinuousConfig = { lookbackDays: 30, minSetupsPerDay: [0, 0.6, 4], minMeanExpectancy: 0.15, adaptiveEnabled: false, study: {
  algorithmId: "future", algorithmVersion: "1", algorithmParams: { threshold: "1.25" }, algorithmSearch: {},
  analyticsParams: { clusterEnabled: true, clusterGapMinutes: 480, timeShiftMinutes: 3, priceChangePct: null }, analyticsSearch: {},
  instruments: [{ exchange: "bybit", category: "linear", symbol: "OLDUSDT" }], direction: "short", horizonMinutes: 10080,
  tpLevels: [4, 6], slLevels: [2, 4], minClosedPerCell: 1, seed: 42, maxTrials: 256, maxSeconds: 3600,
} };
const process: ContinuousProcess = { algorithmId: "future", status: "stopped", configRevision: 1, config, activeStudyId: null, phase: null, error: null, slots: [] };
afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("uses authenticated real endpoints and preserves the date-free configuration", async () => {
  const fetch = vi.fn((path: string) => Promise.resolve(new Response(JSON.stringify(path.includes("?") ? { processes: [process] } : process))));
  vi.stubGlobal("fetch", fetch);
  const signal = new AbortController().signal;
  expect(await listContinuousStudies("future", signal)).toEqual([process]);
  await getContinuousStudy("future", signal);
  await saveContinuousStudy("future", config, signal);
  await startContinuousStudy("future", signal);
  await stopContinuousStudy("future", signal);
  expect(fetch.mock.calls.map(([path]) => path)).toEqual(["/v1/continuous-studies?algorithmId=future", "/v1/continuous-studies/future", "/v1/continuous-studies/future", "/v1/continuous-studies/future/start", "/v1/continuous-studies/future/stop"]);
  const [, init] = vi.mocked(globalThis.fetch).mock.calls[2]!;
  expect(init).toMatchObject({ method: "PUT", signal, credentials: "same-origin" });
  expect(new Headers(init?.headers).get("x-platform-request")).toBe("1");
  const body = init?.body;
  if (typeof body !== "string") throw new Error("Expected JSON request body");
  expect(JSON.parse(body)).toEqual(config);
  expect(config.study).not.toHaveProperty("setupMaturity");
});
it.each([404, 501])("reports endpoint unavailable (%s), not a successful stopped process", async (status) => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("{}", { status }))));
  await expect(listContinuousStudies("future", new AbortController().signal)).rejects.toThrow("пока недоступны");
});
it("rejects malformed successful responses and cross-algorithm mutation responses", async () => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("{}"))));
  await expect(listContinuousStudies("future", new AbortController().signal)).rejects.toThrow("не поддерживает контракт");
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ ...process, algorithmId: "other" })))));
  await expect(startContinuousStudy("future", new AbortController().signal)).rejects.toThrow("неподдерживаемый статус");
});
it("accepts canonical configured and unconfigured endpoint responses", async () => {
  const configured = { ...process, ownerId: "admin", config: { ...config, lookbackDays: 0.5 }, slots: [
    { id: 5, minSetupsPerDay: 4, status: "active", activeGenerationId: 7, studyId: 17, meanExpectancy: 0.21,
      selectedCell: { tpPct: 14, slPct: 4, rawR: 0.32, meanR: 0.21, robustScore: 0.01 }, updatedAt: "2026-11-12T12:00:00Z" },
  ] };
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify(configured)))));
  expect(await getContinuousStudy("future", new AbortController().signal)).toEqual(configured);
  const unconfigured = { algorithmId: "future", status: "stopped", configRevision: 0, config: null, activeStudyId: null, phase: null, error: null, slots: [] };
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify(unconfigured)))));
  expect(await getContinuousStudy("future", new AbortController().signal)).toEqual(unconfigured);
});
const closedBoundary = Date.parse("2026-11-12T12:00:00Z") / 1000;
const incumbentSlot = { id: 5, minSetupsPerDay: 4, status: "warming", activeGenerationId: 7, studyId: 42, meanExpectancy: 0.21,
  selectedCell: { tpPct: 14, slPct: 4, rawR: 0.32, meanR: 0.21, robustScore: 0.01 }, updatedAt: "2026-11-12T12:00:00Z" };
it.each(["warming", "ready", "error", "waiting_resource"])("preserves incumbent, pending %s and exact closed-boundary timestamps", async (pendingStatus) => {
  const configured = { ...process, slots: [{ ...incumbentSlot, activeStatus: "active", pendingGenerationId: 8, pendingStatus,
    readinessFrontier: closedBoundary, pendingError: pendingStatus === "error" ? { code: "history_missing", message: "Missing history" } : null, liveWatermark: 0 }] };
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify(configured)))));
  const response = await getContinuousStudy("future", new AbortController().signal);
  expect(response).toEqual(configured);
  expect(response.slots[0]).toMatchObject({ activeGenerationId: 7, pendingGenerationId: 8, studyId: 42, readinessFrontier: closedBoundary, liveWatermark: 0 });
});
it("accepts explicit inactive/null fields and leaves legacy readiness absent rather than manufacturing ready", async () => {
  const inactive = { ...incumbentSlot, activeGenerationId: null, activeStatus: "inactive", pendingGenerationId: null, pendingStatus: null, readinessFrontier: null, pendingError: null, liveWatermark: null };
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ ...process, slots: [inactive] })))));
  expect((await getContinuousStudy("future", new AbortController().signal)).slots).toEqual([inactive]);
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ ...process, slots: [incumbentSlot] })))));
  const legacy = (await getContinuousStudy("future", new AbortController().signal)).slots[0];
  expect(legacy).not.toHaveProperty("pendingStatus"); expect(legacy).not.toHaveProperty("activeStatus");
});
it.each([
  { activeStatus: "warming" }, { activeStatus: null }, { pendingGenerationId: 0 }, { pendingGenerationId: -1 }, { pendingGenerationId: "8" }, { pendingGenerationId: 1.5 },
  { pendingStatus: 7 }, { readinessFrontier: -60 }, { readinessFrontier: closedBoundary + 1 }, { readinessFrontier: "1794484800" },
  { liveWatermark: -60 }, { liveWatermark: closedBoundary + 0.5 }, { liveWatermark: Number.MAX_SAFE_INTEGER + 1 }, { liveWatermark: "0" },
])("rejects malformed rollout fields %j", async (patch) => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ ...process, slots: [{ ...incumbentSlot, ...patch }] })))));
  await expect(getContinuousStudy("future", new AbortController().signal)).rejects.toThrow("неподдерживаемый статус");
});
it.each(["history unavailable", 0, false, ["history", { detail: "opaque" }]])("preserves opaque JSON pendingError %j without requiring an invented error shape", async (pendingError) => {
  const configured = { ...process, slots: [{ ...incumbentSlot, pendingError }] };
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify(configured)))));
  expect((await getContinuousStudy("future", new AbortController().signal)).slots[0]).toMatchObject({ pendingError });
});

it("preserves both canonical continuous links in detail and compact history without retaining result/events", async () => {
  const detail: OptimizationStudy = { id: 17, origin: "continuous", continuousGenerationId: 7, continuousSlotId: 5, status: "running",
    spec: { ...config.study, fromUnix: 1793491200, toUnix: 1794096000, setupMaturity: 0, minSetupsPerDay: 4 },
    progress: { phase: "acquiring", evaluatedCandidates: 0, scorableCandidates: 0, maxTrials: 256, elapsedSeconds: 1, bestScore: null },
    result: null, error: null, createdAt: "2026-11-12T12:00:00Z", updatedAt: "2026-11-12T12:00:00Z", events: [] };
  const summary = summarizeOptimizationStudy(detail);
  expect(summary).toMatchObject({ origin: "continuous", continuousGenerationId: 7, continuousSlotId: 5 });
  expect(summary).not.toHaveProperty("result"); expect(summary).not.toHaveProperty("events");
  vi.stubGlobal("fetch", vi.fn((path: string) => Promise.resolve(new Response(JSON.stringify(path.includes("?") ? { studies: [summary], total: 1, limit: 1, offset: 0 } : detail)))));
  expect((await getOptimizationStudy(17, new AbortController().signal)).continuousSlotId).toBe(5);
  expect((await listOptimizationStudies("future", new AbortController().signal)).studies[0]).toMatchObject({ origin: "continuous", continuousGenerationId: 7, continuousSlotId: 5 });
});
it("explains canonical referenced-history and owner-stop conflicts without mislabelling them as active/manual jobs", async () => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ error: "continuous_history_referenced" }), { status: 409 }))));
  await expect(deleteOptimizationStudy(17, new AbortController().signal)).rejects.toThrow("связано с адаптивным профилем");
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ error: "continuous_owner_stop_required" }), { status: 409 }))));
  await expect(cancelOptimizationStudy(17, new AbortController().signal)).rejects.toThrow("общую остановку");
});
