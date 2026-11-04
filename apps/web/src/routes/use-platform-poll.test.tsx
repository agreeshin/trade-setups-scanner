import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { usePlatformPoll } from "./use-platform-poll";

afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("polls every three seconds, pauses while hidden and aborts on cleanup", async () => {
  vi.useFakeTimers();
  const visibility = vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
  const signals: AbortSignal[] = [];
  const fetcher = vi.fn((_path: string, init?: RequestInit) => {
    if (init?.signal) signals.push(init.signal);
    return Promise.resolve(new Response(JSON.stringify({ runs: [] })));
  });
  vi.stubGlobal("fetch", fetcher);
  const hook = renderHook(() => usePlatformPoll<{ runs: unknown[] }>("/v1/replay-runs"));
  await act(async () => { await vi.advanceTimersByTimeAsync(0); });
  expect(fetcher).toHaveBeenCalledTimes(1);
  await act(async () => { await vi.advanceTimersByTimeAsync(3000); });
  expect(fetcher).toHaveBeenCalledTimes(2);
  visibility.mockReturnValue("hidden");
  act(() => { document.dispatchEvent(new Event("visibilitychange")); });
  expect(signals[1]?.aborted).toBe(true);
  await act(async () => { await vi.advanceTimersByTimeAsync(9000); });
  expect(fetcher).toHaveBeenCalledTimes(2);
  visibility.mockReturnValue("visible");
  await act(async () => { document.dispatchEvent(new Event("visibilitychange")); await vi.advanceTimersByTimeAsync(0); });
  expect(fetcher).toHaveBeenCalledTimes(3);
  hook.unmount();
  expect(signals[2]?.aborted).toBe(true);
  await vi.advanceTimersByTimeAsync(9000);
  expect(fetcher).toHaveBeenCalledTimes(3);
  expect(vi.getTimerCount()).toBe(0);
});
