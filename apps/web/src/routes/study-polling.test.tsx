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

it("accepts multiple intentional manual submissions and reuses only the uncertain attempt's HTTP-safe key", async () => {
  const getRandomValues = vi.fn(crypto.getRandomValues.bind(crypto));
  vi.stubGlobal("crypto", { getRandomValues });
  const original = globalThis.fetch;
  const creates: { key: string | null; body: unknown }[] = [];
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    if (path === "/v1/optimization-studies" && init?.method === "POST") {
      creates.push({ key: new Headers(init.headers).get("Idempotency-Key"), body: JSON.parse(init.body as string) });
      if (creates.length === 1) return Promise.reject(new TypeError("Lost acknowledgement"));
      const accepted = { ...summary("queued", "manual"), id: 20 + creates.length, result: null, events: [] };
      studies.unshift(accepted); return Promise.resolve(json(accepted));
    }
    return original(path, init);
  }));
  studies = [summary("running", "manual"), { ...summary("running"), id: 18, spec: { ...summary("running").spec, algorithmId: "first" } }];
  const view = renderHook(() => useOptimizationStudies(["first", "second"], true));
  await tick();
  const spec = summary("queued").spec;
  await act(async () => { expect(await view.result.current.start(spec)).toBe(false); });
  await tick();
  expect(creates).toHaveLength(1); expect(creates[0]?.key).toMatch(/^[a-f0-9]{32}$/);
  await act(async () => { expect(await view.result.current.start(spec)).toBe(true); });
  await tick();
  await act(async () => { expect(await view.result.current.start(spec)).toBe(true); });
  expect(creates).toHaveLength(3);
  expect(creates[1]?.key).toBe(creates[0]?.key); expect(creates[2]?.key).not.toBe(creates[1]?.key);
  expect(creates.every((request) => JSON.stringify(request.body) === JSON.stringify(spec))).toBe(true);
  expect(getRandomValues).toHaveBeenCalledTimes(2);
  expect(studies).toHaveLength(4);
  view.unmount(); expect(vi.getTimerCount()).toBe(0);
});

it("keeps manual paused nonterminal at 15 seconds and observes another client's queued Play", async () => {
  studies = [summary("paused", "manual")];
  const view = renderHook(() => useOptimizationStudies(["second"]));
  await tick(); expect(view.result.current.byAlgorithm.second?.status).toBe("paused");
  const before = studyReads(); await tick(14999); expect(studyReads()).toBe(before);
  studies = [summary("queued", "manual")]; await tick(1);
  expect(view.result.current.byAlgorithm.second?.status).toBe("queued");
  await tick(3000); expect(studyReads()).toBe(before + 4);
});

it("serializes manual Pause and releases an unmounted in-flight command", async () => {
  const running = { ...summary("running", "manual"), manualControl: { revision: 3, supportsPause: true } };
  studies = [running];
  const original = globalThis.fetch;
  let finish: (response: Response) => void = () => undefined;
  let signal: AbortSignal | null | undefined;
  const posts: { path: string; body: unknown }[] = [];
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    if (init?.method === "POST") {
      posts.push({ path, body: JSON.parse(init.body as string) }); signal = init.signal;
      return new Promise<Response>((resolve) => { finish = resolve; });
    }
    return original(path, init);
  }));
  const view = renderHook(() => useOptimizationStudies(["second"])); await tick();
  let pending: Promise<boolean> = Promise.resolve(false);
  act(() => { pending = view.result.current.pause(running); });
  expect(view.result.current.controlling).toEqual({ id: 17, action: "pause" });
  expect(await view.result.current.pause(running)).toBe(false);
  expect(await view.result.current.stop(running)).toBe(false);
  expect(posts).toEqual([{ path: "/v1/optimization-studies/17/pause", body: { expectedControlRevision: 3 } }]);
  view.unmount(); expect(signal?.aborted).toBe(true); expect(vi.getTimerCount()).toBe(0);
  await act(async () => { finish(json({ ...running, status: "pausing", result: null, events: [] })); expect(await pending).toBe(false); });
});

it("handles manual Play 409 with one exact-ID GET, preserving progress without another mutation", async () => {
  const paused = { ...summary("paused", "manual"), manualControl: { revision: 3, supportsPause: true }, progress: { ...summary("paused").progress, evaluatedCandidates: 7, elapsedSeconds: 37 } };
  studies = [paused];
  const original = globalThis.fetch;
  const operations: string[] = [];
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    if (path.endsWith("/play")) {
      operations.push(path);
      expect(JSON.parse(init?.body as string)).toEqual({ expectedControlRevision: 3 });
      return Promise.resolve(new Response(JSON.stringify({ error: "manual_control_revision_changed" }), { status: 409 }));
    }
    if (path === "/v1/optimization-studies/17") {
      operations.push(path); studies = [{ ...paused, status: "queued", manualControl: { revision: 4, supportsPause: true } }];
      return Promise.resolve(json({ ...studies[0], result: null, events: [] }));
    }
    return original(path, init);
  }));
  const view = renderHook(() => useOptimizationStudies(["second"])); await tick();
  await act(async () => { expect(await view.result.current.play(paused)).toBe(false); }); await tick();
  expect(operations).toEqual(["/v1/optimization-studies/17/play", "/v1/optimization-studies/17"]);
  expect(view.result.current.byAlgorithm.second).toMatchObject({ id: 17, status: "queued", progress: { evaluatedCandidates: 7, elapsedSeconds: 37 } });
  expect(view.result.current.controlling).toBeNull(); expect(view.result.current.error).toMatch(/Состояние исследования изменилось/);
});

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

it("observes running → pausing at three seconds → paused at fifteen seconds and releases its timer", async () => {
  processes.first = { ...emptyProcess(), status: "running", desiredRunning: true, activeStudyId: 17 };
  const view = renderHook(() => useContinuousStudy("first"));
  await tick(); expect(continuousReads()).toBe(1);
  processes.first = { ...processes.first, pauseRequested: true, pauseState: "pausing" };
  await tick(3000); expect(continuousReads()).toBe(2);
  expect(view.result.current.process?.pauseState).toBe("pausing");
  const pausingRevision = view.result.current.revision;
  expect(pausingRevision).toBeGreaterThan(0);
  processes.first = { ...processes.first, pauseState: "paused", activeStudyId: null };
  await tick(3000); expect(continuousReads()).toBe(3);
  expect(view.result.current.process?.pauseState).toBe("paused");
  expect(view.result.current.revision).toBeGreaterThan(pausingRevision);
  await tick(14999); expect(continuousReads()).toBe(3);
  await tick(1); expect(continuousReads()).toBe(4);
  view.unmount(); expect(vi.getTimerCount()).toBe(0);
});

it("keeps an errored pausing chain visible to recovery observation rather than treating it as idle", async () => {
  processes.first = { ...emptyProcess(), status: "error", desiredRunning: true, pauseRequested: true, pauseState: "pausing", activeStudyId: 17, error: { code: "source_unavailable", message: "Retry pending" } };
  renderHook(() => useContinuousStudy("first"));
  await tick(); await tick(3000);
  expect(continuousReads()).toBe(2);
});

it("serializes Pause with other commands and fences a late pre-pause read before observing the ACK", async () => {
  const running: ContinuousProcess = { ...emptyProcess(), status: "running", desiredRunning: true, activeStudyId: 17 };
  processes.first = running;
  const originalFetch = globalThis.fetch;
  let resolveOldRead: ((response: Response) => void) | undefined;
  let resolvePause: ((response: Response) => void) | undefined;
  let delayRead = false;
  const commandPaths: string[] = [];
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    if (init?.method === "POST") {
      commandPaths.push(path);
      return new Promise<Response>((resolve) => { resolvePause = resolve; });
    }
    if (delayRead) {
      delayRead = false;
      return new Promise<Response>((resolve) => { resolveOldRead = resolve; });
    }
    return originalFetch(path, init);
  }));
  const view = renderHook(() => useContinuousStudy("first"));
  await tick();
  delayRead = true;
  act(() => { view.result.current.refresh(); });
  let pauseCommand: Promise<boolean> | undefined;
  act(() => { pauseCommand = view.result.current.pause(); });
  expect(view.result.current.busy).toBe("pause");
  expect(await view.result.current.pause()).toBe(false);
  expect(await view.result.current.stop()).toBe(false);
  const pausing: ContinuousProcess = { ...running, pauseRequested: true, pauseState: "pausing" };
  processes.first = pausing;
  await act(async () => { resolveOldRead?.(json({ processes: [{ ...running, configRevision: 999 }] })); await Promise.resolve(); });
  expect(view.result.current.process).toEqual(running);
  await act(async () => { resolvePause?.(json(pausing)); expect(await pauseCommand).toBe(true); });
  expect(view.result.current.process).toEqual(pausing);
  expect(view.result.current.busy).toBeNull();
  expect(commandPaths).toEqual(["/v1/continuous-studies/first/pause"]);
  expect(continuousReads()).toBe(2);
  view.unmount(); expect(vi.getTimerCount()).toBe(0);
});

it("aborts an in-flight Pause on unmount and does not retain observer timers or accept its late ACK", async () => {
  const running: ContinuousProcess = { ...emptyProcess(), status: "running", activeStudyId: 17 };
  processes.first = running;
  const originalFetch = globalThis.fetch;
  let resolvePause: ((response: Response) => void) | undefined;
  let commandSignal: AbortSignal | null | undefined;
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    if (init?.method === "POST") {
      commandSignal = init.signal;
      return new Promise<Response>((resolve) => { resolvePause = resolve; });
    }
    return originalFetch(path, init);
  }));
  const view = renderHook(() => useContinuousStudy("first"));
  await tick();
  let command: Promise<boolean> | undefined;
  act(() => { command = view.result.current.pause(); });
  expect(commandSignal?.aborted).toBe(false);
  view.unmount();
  expect(commandSignal?.aborted).toBe(true);
  expect(vi.getTimerCount()).toBe(0);
  await act(async () => { resolvePause?.(json({ ...running, pauseRequested: true, pauseState: "pausing" })); expect(await command).toBe(false); });
  expect(continuousReads()).toBe(1);
  expect(vi.getTimerCount()).toBe(0);
});

it.each([
  { status: "error" as const, error: { code: "source_unavailable", message: "Retry pending" } },
  { desiredRunning: true }, { activeStudyId: 17 },
])("keeps three-second observation for known retry/desired/pending work: %j", async (patch) => {
  processes.first = { ...emptyProcess(), ...patch };
  renderHook(() => useContinuousStudy("first"));
  await tick(); await tick(3000); expect(continuousReads()).toBe(2);
});

it.each(["warming", "ready"])("does not accelerate stopped research because an independent profile is %s", async (pendingStatus) => {
  processes.first = { ...emptyProcess(), slots: [{ ...emptySlot, pendingGenerationId: 8, pendingStatus }] };
  renderHook(() => useContinuousStudy("first"));
  await tick(); await tick(3000); expect(continuousReads()).toBe(1);
  await tick(12000); expect(continuousReads()).toBe(2);
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
