import { StrictMode } from "react";
import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { useSetupOutcomesRefresh } from "./use-setup-outcomes-refresh";

const json = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status });
const page = (nextCursor: string | null = null) => ({ snapshotId: 100, observedThroughUnix: 360, processed: 2, computed: 1, updated: 1, nextCursor });
const requests: { body: { cursor?: string; limit: number }; signal: AbortSignal | null | undefined }[] = [];
let respond: (cursor: string | undefined) => Response | Promise<Response>;
const settle = async () => { await act(async () => { await vi.advanceTimersByTimeAsync(0); }); };

beforeEach(() => {
  vi.useFakeTimers(); requests.length = 0; respond = () => json(page());
  vi.stubGlobal("fetch", vi.fn((_path: string, init: RequestInit) => {
    if (typeof init.body !== "string") throw new Error("Expected a JSON request body");
    const body = JSON.parse(init.body) as { cursor?: string; limit: number };
    requests.push({ body, signal: init.signal });
    return new Promise<Response>((resolve, reject) => {
      init.signal?.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError")), { once: true });
      Promise.resolve(respond(body.cursor)).then(resolve, reject);
    });
  }));
});
afterEach(async () => { cleanup(); await vi.advanceTimersByTimeAsync(0); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("runs serial pages exactly once under StrictMode and retains only accumulated scalars", async () => {
  let finish: ((value: Response) => void) | undefined;
  respond = (cursor) => cursor === undefined ? new Promise<Response>((resolve) => { finish = resolve; }) : json(page());
  const hook = renderHook(() => useSetupOutcomesRefresh(true), { wrapper: StrictMode });
  await settle(); expect(requests).toHaveLength(1); expect(requests[0]?.signal?.aborted).toBe(false);
  expect(hook.result.current.pending).toBe(true);
  await act(async () => { finish?.(json(page("next"))); await Promise.resolve(); }); await settle();
  expect(requests.map((request) => request.body)).toEqual([{ limit: 50 }, { cursor: "next", limit: 50 }]);
  expect(hook.result.current).toMatchObject({ pending: false, processed: 4, computed: 2, updated: 2, error: null, revision: 1 });
  expect(vi.getTimerCount()).toBe(0);
});

it("waits for the first active entry and never recalculates on later activation or a timer", async () => {
  const hook = renderHook(({ active }) => useSetupOutcomesRefresh(active), { initialProps: { active: false } });
  await settle(); expect(requests).toHaveLength(0);
  hook.rerender({ active: true }); await settle(); expect(requests).toHaveLength(1);
  hook.rerender({ active: false }); await settle(); hook.rerender({ active: true }); await settle();
  await act(async () => { await vi.advanceTimersByTimeAsync(600_000); });
  expect(requests).toHaveLength(1);
});

it("keeps the command alive when the keep-alive route hides and blocks rapid manual overlap", async () => {
  let finish: ((value: Response) => void) | undefined;
  respond = () => new Promise<Response>((resolve) => { finish = resolve; });
  const hook = renderHook(({ active }) => useSetupOutcomesRefresh(active), { initialProps: { active: true } });
  await settle(); hook.rerender({ active: false }); await settle();
  expect(requests[0]?.signal?.aborted).toBe(false);
  act(() => { void hook.result.current.refresh(); void hook.result.current.refresh(); }); expect(requests).toHaveLength(1);
  await act(async () => { finish?.(json(page())); await Promise.resolve(); }); await settle();
  respond = () => json(page());
  act(() => { void hook.result.current.refresh(); void hook.result.current.refresh(); }); await settle();
  expect(requests).toHaveLength(2); expect(requests[1]?.body.cursor).toBeUndefined();
  hook.rerender({ active: true }); await settle(); expect(requests).toHaveLength(2);
});

it("retains partial progress on error, does not automatically retry, and allows explicit retry", async () => {
  respond = (cursor) => cursor === undefined ? json(page("next")) : json({}, 503);
  const hook = renderHook(({ active }) => useSetupOutcomesRefresh(active), { initialProps: { active: true } }); await settle();
  expect(hook.result.current).toMatchObject({ pending: false, processed: 2, computed: 1, updated: 1, revision: 1 });
  expect(hook.result.current.error).toContain("HTTP 503");
  hook.rerender({ active: false }); hook.rerender({ active: true }); await settle(); expect(requests).toHaveLength(2);
  respond = () => json(page()); await act(async () => { await hook.result.current.refresh(); });
  expect(requests).toHaveLength(3); expect(requests[2]?.body).toEqual({ limit: 50 });
  expect(hook.result.current).toMatchObject({ pending: false, processed: 2, error: null, revision: 2 });
});

it("aborts on a genuine unmount without issuing the next page after a late response", async () => {
  let finish: ((value: Response) => void) | undefined;
  respond = () => new Promise<Response>((resolve) => { finish = resolve; });
  const hook = renderHook(() => useSetupOutcomesRefresh(true), { wrapper: StrictMode }); await settle();
  hook.unmount(); await settle(); expect(requests[0]?.signal?.aborted).toBe(true);
  await act(async () => { finish?.(json(page("next"))); await Promise.resolve(); }); await settle();
  expect(requests).toHaveLength(1); expect(vi.getTimerCount()).toBe(0);
});

it("exposes a finite batch timeout without scheduling another calculation", async () => {
  respond = () => new Promise<Response>(() => undefined);
  const hook = renderHook(() => useSetupOutcomesRefresh(true)); await settle();
  await act(async () => { await vi.advanceTimersByTimeAsync(120_000); });
  expect(hook.result.current.pending).toBe(false); expect(hook.result.current.error).toContain("120 секунд");
  await act(async () => { await vi.advanceTimersByTimeAsync(600_000); });
  expect(requests).toHaveLength(1); expect(vi.getTimerCount()).toBe(0);
});

it("rejects a changing snapshot and resets the once flag only with a fresh page owner", async () => {
  respond = (cursor) => json(cursor === undefined ? page("next") : { ...page(), snapshotId: 101 });
  const hook = renderHook(() => useSetupOutcomesRefresh(true)); await settle();
  expect(hook.result.current.error).toContain("Изменился снимок");
  hook.unmount(); await settle(); respond = () => json(page());
  renderHook(() => useSetupOutcomesRefresh(true)); await settle(); expect(requests).toHaveLength(3);
});
