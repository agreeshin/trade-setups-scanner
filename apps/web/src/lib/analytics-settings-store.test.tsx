import { StrictMode, type PropsWithChildren } from "react";
import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { loadSharedAnalyticsSettings, saveSharedAnalyticsSettings, updateSharedAnalyticsSettings, useSharedAnalyticsSettings } from "./analytics-settings-store";
import { applyInterfaceSettings } from "./datetime-format";

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });
const original = { setId: "run-12", trendMode: "forecast", trendBendDays: "14", trendBlendLambda: "0.7",
  forecast: { method: "decayed_probability", version: 1, halfLifeDays: "14", kappa: "10", priorPolicy: "gross_break_even" },
  sections: { "long-matrix": { untouched: "keep", timeShiftInput: "13" }, futureSection: { keep: true } }, futureRoot: { keep: "unknown" } };
let get: () => Response | Promise<Response>;
let put: (body: Record<string, unknown>) => Response | Promise<Response>;
let calls: { method: string; body: Record<string, unknown> | null }[];
beforeEach(() => {
  vi.useFakeTimers(); calls = [];
  get = () => json({ settings: original }); put = () => json({ settings: {} });
  vi.stubGlobal("fetch", vi.fn((url: string, init?: RequestInit) => {
    if (url !== "/v1/settings/web-analytics") throw new Error(`Unexpected settings key ${url}`);
    const method = init?.method ?? "GET";
    const body = typeof init?.body === "string" ? JSON.parse(init.body) as Record<string, unknown> : null;
    calls.push({ method, body });
    return Promise.resolve(method === "PUT" ? put(body!) : get());
  }));
});
afterEach(async () => { cleanup(); await Promise.resolve(); vi.clearAllTimers(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); applyInterfaceSettings({ timezone: "auto" }); });
const settle = async (ms = 0) => { await act(async () => { await vi.advanceTimersByTimeAsync(ms); }); };
const puts = () => calls.filter((call) => call.method === "PUT");

it("shares one load and one forecast owner across StrictMode/keep-alive consumers and preserves unrelated settings", async () => {
  const one = renderHook(useSharedAnalyticsSettings, { wrapper: ({ children }: PropsWithChildren) => <StrictMode>{children}</StrictMode> });
  const two = renderHook(useSharedAnalyticsSettings);
  await settle();
  expect(calls.filter((call) => call.method === "GET")).toHaveLength(1);
  act(() => updateSharedAnalyticsSettings({ trendMode: "blend", trendBlendLambda: "0.4" }));
  expect(one.result.current.settings).toBe(two.result.current.settings);
  expect(two.result.current.settings?.trendBlendLambda).toBe("0.4");
  expect(puts()).toHaveLength(0); await settle(800);
  expect(puts()).toHaveLength(1);
  expect(puts()[0]?.body).toMatchObject({ trendMode: "blend", trendBlendLambda: "0.4", sections: original.sections, futureRoot: original.futureRoot });
  expect(two.result.current.saveStatus).toBe("saved");
});

it.each([
  { mode: "custom", month: "custom", fromDay: "2026-03-08", toDay: "2026-03-08" },
  { mode: "month", month: "2026-03", fromDay: null, toDay: null },
])("preserves untouched legacy $mode range fields when shared forecast and other settings are saved", async (timeRange) => {
  applyInterfaceSettings({ timezone: "America/New_York" });
  const sections = { ...original.sections, "short-matrix": { timeRange, timeShiftInput: "0" } };
  get = () => json({ settings: { ...original, sections } });
  const owner = renderHook(useSharedAnalyticsSettings); await settle();
  expect(owner.result.current.settings?.sections["short-matrix"]?.timeRange).toEqual(timeRange);
  act(() => updateSharedAnalyticsSettings({ trendMode: "blend", trendBlendLambda: "0.4" })); await settle(800);
  expect(puts()).toHaveLength(1);
  expect(puts()[0]?.body).toMatchObject({ trendMode: "blend", trendBlendLambda: "0.4", sections });
  act(() => updateSharedAnalyticsSettings({ clusterGapInput: "9", sections: { "short-matrix": { gradientMin: -0.1 } } })); await settle(800);
  expect(puts()).toHaveLength(2);
  expect(puts()[1]?.body).toMatchObject({ clusterGapInput: "9", trendBlendLambda: "0.4",
    sections: { ...sections, "short-matrix": { ...sections["short-matrix"], gradientMin: -0.1 } } });
});

it("serializes PUTs and ignores an older save ACK/refetch while a newer control edit is queued", async () => {
  const replies: ((response: Response) => void)[] = [];
  put = () => new Promise<Response>((resolve) => { replies.push(resolve); });
  const one = renderHook(useSharedAnalyticsSettings); const two = renderHook(useSharedAnalyticsSettings);
  await settle();
  act(() => updateSharedAnalyticsSettings({ trendBlendLambda: "0.4" })); await settle(800);
  act(() => { updateSharedAnalyticsSettings({ trendBlendLambda: "0.8" }); loadSharedAnalyticsSettings(); });
  await settle(1600);
  expect(puts()).toHaveLength(1); expect(calls.filter((call) => call.method === "GET")).toHaveLength(1);
  expect(two.result.current.settings?.trendBlendLambda).toBe("0.8");
  await act(async () => { replies[0]!(json({ settings: { trendBlendLambda: "0.4" } })); await Promise.resolve(); });
  expect(puts()).toHaveLength(2);
  expect(puts()[1]?.body?.trendBlendLambda).toBe("0.8");
  expect(one.result.current.saveStatus).toBe("saving");
  expect(one.result.current.settings?.trendBlendLambda).toBe("0.8");
  await act(async () => { replies[1]!(json({ settings: { trendBlendLambda: "0.4" } })); await Promise.resolve(); });
  expect(two.result.current.settings?.trendBlendLambda).toBe("0.8"); expect(two.result.current.saveStatus).toBe("saved");
});

it("fails closed on saved unknown recipes, without replacing them with defaults or allowing writes", async () => {
  get = () => json({ settings: { ...original, forecast: { method: "future", version: 99 } } });
  const owner = renderHook(useSharedAnalyticsSettings); await settle();
  expect(owner.result.current.status).toBe("error"); expect(owner.result.current.settings).toBeNull();
  act(() => updateSharedAnalyticsSettings({ trendMode: "mean" })); await settle(2000);
  expect(puts()).toHaveLength(0);
  get = () => json({ settings: { trendMode: "forecast" } });
  act(loadSharedAnalyticsSettings); await settle();
  expect(owner.result.current.status).toBe("ready"); expect(owner.result.current.settings?.forecast).toBeUndefined();
});

it("surfaces save failure, retains the latest draft and retries only on an explicit request", async () => {
  put = () => json({ error: "offline" }, 503);
  const owner = renderHook(useSharedAnalyticsSettings); await settle();
  act(() => updateSharedAnalyticsSettings({ trendBlendLambda: "0.6" })); await settle(800);
  expect(owner.result.current.saveStatus).toBe("error"); expect(owner.result.current.saveError).toContain("503");
  expect(owner.result.current.settings?.trendBlendLambda).toBe("0.6"); await settle(5000); expect(puts()).toHaveLength(1);
  put = () => json({}); act(saveSharedAnalyticsSettings); await settle();
  expect(owner.result.current.saveStatus).toBe("saved"); expect(puts()).toHaveLength(2);
});

it("releases the last subscriber/load/timer instead of retaining drafts between app lifetimes", async () => {
  const first = renderHook(useSharedAnalyticsSettings); await settle();
  act(() => updateSharedAnalyticsSettings({ trendMode: "blend" })); first.unmount(); await settle(5000);
  expect(puts()).toHaveLength(0);
  const next = renderHook(useSharedAnalyticsSettings); await settle();
  expect(calls.filter((call) => call.method === "GET")).toHaveLength(2);
  expect(next.result.current.settings?.trendMode).toBe("forecast");
});

it("fences an old lifetime's in-flight ACK and continues the new owner's queued writer", async () => {
  let reply: ((response: Response) => void) | undefined;
  put = () => new Promise<Response>((resolve) => { reply = resolve; });
  const first = renderHook(useSharedAnalyticsSettings); await settle();
  act(() => updateSharedAnalyticsSettings({ trendMode: "blend" })); await settle(800);
  first.unmount(); await settle();
  get = () => json({ settings: { ...original, trendMode: "mean" } });
  const next = renderHook(useSharedAnalyticsSettings); await settle();
  act(() => updateSharedAnalyticsSettings({ trendBlendLambda: "0.9" })); await settle(800);
  put = () => json({}); await act(async () => { reply!(json({})); await Promise.resolve(); });
  expect(next.result.current.settings).toMatchObject({ trendMode: "mean", trendBlendLambda: "0.9" });
  expect(puts()).toHaveLength(2); expect(puts()[1]?.body).toMatchObject({ trendMode: "mean", trendBlendLambda: "0.9" });
});

it("merges section fields without losing unrelated per-section/unknown data or newer shared forecast fields", async () => {
  const owner = renderHook(useSharedAnalyticsSettings); await settle();
  act(() => updateSharedAnalyticsSettings({ trendMode: "blend", sections: { "long-matrix": { timeShiftInput: "14" } } }));
  act(() => updateSharedAnalyticsSettings({ clusterGapInput: "20", sections: { "short-matrix": { gradientMin: -0.1 } } }));
  await settle(800);
  expect(owner.result.current.settings).toMatchObject({ trendMode: "blend", clusterGapInput: "20",
    sections: { "long-matrix": { untouched: "keep", timeShiftInput: "14" }, "short-matrix": { gradientMin: -0.1 }, futureSection: { keep: true } } });
  expect(puts()[0]?.body?.futureRoot).toEqual(original.futureRoot);
});

it("retains the previous Analytics effective bend when a saved explicit trend recipe overrides an inactive legacy bend", async () => {
  get = () => json({ settings: { ...original, trendBendDays: "7", forecast: { method: "trend", version: 1, bendDays: "30" } } });
  const owner = renderHook(useSharedAnalyticsSettings); await settle();
  expect(owner.result.current.settings?.trendBendDays).toBe("30"); expect(puts()).toHaveLength(0);
});

it("restores the additive sum recipe and shared bend without clobbering unknown settings or retaining retired list filters", async () => {
  get = () => json({ settings: { ...original, trendBendDays: "7", forecast: { method: "trend_sum_r", version: 1, bendDays: "30" },
    sections: { ...original.sections, "short-trades": { tpInput: "8", slInput: "2", riskPctInput: "3", futureCell: "keep",
      timeRange: { mode: "month", month: "1999-01" }, timeShiftInput: "999", considerPriceChange: true, priceChangeInput: "99" } } } });
  const owner = renderHook(useSharedAnalyticsSettings); await settle();
  expect(owner.result.current.settings).toMatchObject({ trendBendDays: "30", forecast: { method: "trend_sum_r", version: 1, bendDays: "30" } });
  expect(owner.result.current.settings?.sections["short-trades"]).toEqual({ tpInput: "8", slInput: "2", riskPctInput: "3", futureCell: "keep" });
  act(() => updateSharedAnalyticsSettings({ sections: { "long-matrix": { timeShiftInput: "4" } } })); await settle(800);
  expect(puts()[0]?.body).toMatchObject({ forecast: { method: "trend_sum_r", version: 1, bendDays: "30" }, futureRoot: original.futureRoot });
  expect(puts()[0]?.body?.sections).toMatchObject({ "long-matrix": { timeShiftInput: "4", untouched: "keep" },
    "short-trades": { tpInput: "8", slInput: "2", riskPctInput: "3", futureCell: "keep" } });
  expect(JSON.stringify(puts()[0]?.body)).not.toContain("1999-01");
});
