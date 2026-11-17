import { expect, it } from "vitest";
import type { TradeRow } from "./analytics-api";
import { buildObservedForecastDays } from "./forecast-observations";

const day = Date.parse("2026-01-01T00:00:00Z") / 1000;
function row(signal: number, exit: number | null, status = "tp"): TradeRow & { fromCache: boolean } {
  return { setup: { exchange: "bybit", category: "linear", symbol: "BTCUSDT", openTimeUnix: signal - 60, closeTimeUnix: signal, closePrice: "100" },
    direction: "short", tpPct: 2, slPct: 1, status, entryPrice: 100, tpPrice: 98, slPrice: 101,
    closeTimeUnix: exit, riskReturn: status === "tp" ? 2 : status === "sl" ? -1 : null, fromCache: false };
}

it("counts only actual TP/SL labels available at C, using exit-candle open +60, not rounded daily means", () => {
  const cutoff = day + 2 * 86400;
  const days = buildObservedForecastDays([
    row(day + 60, cutoff - 60), row(day + 120, cutoff - 61, "sl"),
    row(day + 180, cutoff - 59), row(day + 240, null), row(day + 300, cutoff + 60, "sl"),
    row(day + 86400 + 60, cutoff - 120, "unclosed"), row(day + 86400 + 120, cutoff - 120, "no_data"),
    row(day - 1, cutoff - 120), row(cutoff + 1, cutoff - 120), row(cutoff + 86400, cutoff - 120),
  ], day, cutoff + 86400, cutoff);
  expect(days.map((item) => [item.dayStartUnix, item.tpCount, item.slCount, item.closedCount, item.riskExpectancy])).toEqual([
    [day, 1, 1, 2, 0.5], [day + 86400, 0, 0, 0, null],
  ]);
});

it("keeps raw-signal UTC cohorts even when the known exit falls on another day, without mutating rows", () => {
  const late = row(day + 60, day + 2 * 86400);
  const original = structuredClone(late);
  expect(buildObservedForecastDays([late], day, day + 86400, day + 3 * 86400)[0]).toMatchObject({ dayStartUnix: day, tpCount: 1 });
  expect(buildObservedForecastDays([late], day, day + 86400, day + 86400)[0]).toMatchObject({ dayStartUnix: day, tpCount: 0, riskExpectancy: null });
  expect(late).toEqual(original);
});

it("rejects invalid source bounds instead of presenting fabricated history", () => {
  expect(() => buildObservedForecastDays([], day, day, day)).toThrow("границы");
  expect(() => buildObservedForecastDays([], day, day + 60, NaN)).toThrow("cutoff");
});
