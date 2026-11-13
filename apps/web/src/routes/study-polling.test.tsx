import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { PropsWithChildren } from "react";
import type { ContinuousProcess, ContinuousSlot } from "../lib/continuous-api";
import type { OptimizationStudySummary } from "../lib/optimization-api";
import { RouteActiveContext } from "./route-active";
import { useContinuousStudy } from "./use-continuous-study";
import { useOptimizationStudies } from "./use-optimization-studies";

const emptyProcess = (algorithmId = "first"): ContinuousProcess => ({ algorithmId, status: "stopped", configRevision: 1, config: null, activeStudyId: null, phase: null, error: null, slots: [] });
const emptySlot: ContinuousSlot = { id: 1, minSetupsPerDay: 4, status: "stopped", activeGenerationId: null, studyId: null, meanExpectancy: null, selectedCell: null, updatedAt: "2026-11-13T12:00:00Z" };
function summary(status: OptimizationStudySummary["status"], origin: "manual" | "continuous" = "continuous"): OptimizationStudySummary {
  return { id: 17, status, origin, spec: { algorithmId: "second", algorithmVersion: "1", algorithmParams: {}, algorithmSearch: {}, analyticsParams: { clusterEnabled: false, clusterGapMinutes: 480, timeShiftMinutes: 0, priceChangePct: null }, analyticsSearch: {}, instruments: [], fromUnix: 0, toUnix: 60, direction: "short", setupMaturity: 0, horizonMinutes: 60, tpLevels: [4], slLevels: [2], minClosedPerCell: 1, seed: 42, maxTrials: 256, maxSeconds: 3600 }, progress: { phase: "acquiring", evaluatedCandidates: 0, scorableCandidates: 0, maxTrials: 256, elapsedSeconds: 0, bestScore: null }, bestScore: null, error: null, createdAt: "2026-11-13T12:00:00Z", updatedAt: "2026-11-13T12:00:00Z" };
}
let studies: OptimizationStudySummary[];
let processes: Record<string, ContinuousProcess>;
const calls: { path: string; signal: AbortSignal | null | undefined }[] = [];
const json = (value: unknown) => new Response(JSON.stringify(value));
const tick = async (ms = 0) => { await act(async () => { await vi.advanceTimersByTimeAsync(ms); }); };
const studyReads = () => calls.filter((call) => call.path.startsWith("/v1/optimization-studies?")).length;
const continuousReads = () => calls.filter((call) => call.path.startsWith("/v1/continuous-studies?")).length;
beforeEach(() => {
  vi.useFakeTimers(); studies = []; processes = { first: emptyProcess(), second: emptyProcess("second") }; calls.length = 0;
  vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    calls.push({ path, signal: init?.signal });
    const query = new URL(path, "http://localhost").searchParams;
    const id = query.get("algorithmId");
    if (path.startsWith("/v1/continuous-studies?")) return Promise.resolve(json({ processes: id && processes[id] ? [processes[id]] : [] }));
    const filtered = studies.filter((study) => !id || study.spec.algorithmId === id);
    return Promise.resolve(json({ studies: filtered, total: filtered.length, limit: Number(query.get("limit")), offset: 0 }));
  }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("keeps the two-algorithm idle baseline at five reads per 15 seconds, not every three seconds", async () => {
  const view = renderHook(() => { useContinuousStudy("first"); useContinuousStudy("second"); return useOptimizationStudies(["first", "second"]); });
  await tick(); expect(calls).toHaveLength(5); expect(vi.getTimerCount()).toBe(3);
  await tick(14999); expect(calls).toHaveLength(5);
  await tick(1); expect(calls).toHaveLength(10);
  await tick(45000); expect(calls).toHaveLength(25);
  view.unmount(); expect(vi.getTimerCount()).toBe(0);
});

it.each(["queued", "running", "cancel_requested"] as const)("changes shared polling idle → continuous %s on another algorithm → idle without occupying manual admission", async (status) => {
  const view = renderHook(() => useOptimizationStudies(["first", "second"]));
  await tick(); expect(studyReads()).toBe(3);
  studies = [summary(status)];
  await tick(15000); expect(studyReads()).toBe(6); expect(view.result.current.active).toBeNull();
  expect(view.result.current.latestByAlgorithm.second?.status).toBe(status);
  await tick(2999); expect(studyReads()).toBe(6);
  await tick(1); expect(studyReads()).toBe(9);
  studies = [summary("completed")];
  await tick(3000); expect(studyReads()).toBe(12);
  await tick(14999); expect(studyReads()).toBe(12);
  await tick(1); expect(studyReads()).toBe(15);
});

it("changes a continuous observer idle → running between children → stopping → idle", async () => {
  renderHook(() => useContinuousStudy("first"));
  await tick(); expect(continuousReads()).toBe(1);
  processes.first = { ...emptyProcess(), status: "running", phase: "queued" };
  await tick(15000); expect(continuousReads()).toBe(2);
  await tick(3000); expect(continuousReads()).toBe(3);
  processes.first = { ...emptyProcess(), status: "stopping", phase: "cleanup" };
  await tick(3000); expect(continuousReads()).toBe(4);
  processes.first = emptyProcess();
  await tick(3000); expect(continuousReads()).toBe(5);
  await tick(14999); expect(continuousReads()).toBe(5);
  await tick(1); expect(continuousReads()).toBe(6);
});

it.each([
  { status: "error" as const, error: { code: "source_unavailable", message: "Retry pending" } },
  { desiredRunning: true }, { activeStudyId: 17 },
  { slots: [{ ...emptySlot, pendingGenerationId: 8, pendingStatus: "warming" }] },
  { slots: [{ ...emptySlot, pendingGenerationId: 8, pendingStatus: "ready" }] },
])("keeps three-second observation for known retry/desired/pending work: %j", async (patch) => {
  processes.first = { ...emptyProcess(), ...patch };
  renderHook(() => useContinuousStudy("first"));
  await tick(); await tick(3000); expect(continuousReads()).toBe(2);
});

it("pauses both observers on hidden document/inactive route, refreshes immediately on return, and releases timers", async () => {
  let active = true;
  const visibility = vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
  const wrapper = ({ children }: PropsWithChildren) => <RouteActiveContext.Provider value={active}>{children}</RouteActiveContext.Provider>;
  const view = renderHook(() => { useContinuousStudy("first"); return useOptimizationStudies(["first"]); }, { wrapper });
  await tick(); expect(calls).toHaveLength(3);
  visibility.mockReturnValue("hidden"); act(() => { document.dispatchEvent(new Event("visibilitychange")); });
  await tick(60000); expect(calls).toHaveLength(3); expect(vi.getTimerCount()).toBe(0);
  expect(calls.every((call) => call.signal?.aborted)).toBe(true);
  visibility.mockReturnValue("visible"); act(() => { document.dispatchEvent(new Event("visibilitychange")); });
  await tick(); expect(calls).toHaveLength(6);
  active = false; view.rerender();
  await tick(60000); expect(calls).toHaveLength(6); expect(vi.getTimerCount()).toBe(0);
  active = true; view.rerender(); await tick(); expect(calls).toHaveLength(9);
  view.unmount(); await tick(60000); expect(calls).toHaveLength(9); expect(vi.getTimerCount()).toBe(0);
});

it("starts the idle interval after a slow shared batch settles, never overlapping periodic reads", async () => {
  const original = globalThis.fetch;
  let resolveSlow: ((response: Response) => void) | undefined;
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    if (path.includes("algorithmId=second") && !resolveSlow) {
      calls.push({ path, signal: init?.signal });
      return new Promise<Response>((resolve) => { resolveSlow = resolve; });
    }
    return original(path, init);
  }));
  renderHook(() => useOptimizationStudies(["first", "second"]));
  await tick(); await tick(45000); expect(studyReads()).toBe(3);
  await act(async () => { resolveSlow?.(json({ studies: [], total: 0, limit: 20, offset: 0 })); await Promise.resolve(); });
  await tick(14999); expect(studyReads()).toBe(3);
  await tick(1); expect(studyReads()).toBe(6);
});
