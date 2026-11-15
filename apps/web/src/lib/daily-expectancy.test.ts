import { expect, it } from "vitest";
import type { TradeRow } from "./analytics-api";
import { buildDailyExpectancy, buildDailyExpectancyModel, utcDayStart, type DailyExpectancyPoint } from "./daily-expectancy";

const unix = (value: string) => Date.parse(value) / 1000;
function row(at: string, riskReturn: number | null, status = riskReturn === null ? "unclosed" : riskReturn < 0 ? "sl" : "tp"): TradeRow & { fromCache: boolean } {
  const closeTimeUnix = unix(at);
  return { setup: { exchange: "bybit", category: "linear", symbol: "BTCUSDT", openTimeUnix: closeTimeUnix - 60, closeTimeUnix, closePrice: "100" },
    direction: "short", tpPct: 4, slPct: 4, status, entryPrice: 100, tpPrice: 96, slPrice: 104,
    // Trade exit is deliberately on another day; grouping must use signal close.
    closeTimeUnix: closeTimeUnix + 86400, riskReturn, fromCache: true };
}
function point(day: string, riskExpectancy: number | null): DailyExpectancyPoint {
  return { day, dayStartUnix: unix(`${day}T00:00:00Z`), riskExpectancy, closedCount: riskExpectancy === null ? 0 : 1,
    tpCount: riskExpectancy === null || riskExpectancy < 0 ? 0 : 1, slCount: riskExpectancy !== null && riskExpectancy < 0 ? 1 : 0, computedCount: 1, cachedCount: 0 };
}

it("matches UTC daily buckets and setup weights for the shared Rust trend solver", () => {
  const rows = [row("2026-01-01T00:00:00Z", 2), row("2026-01-01T12:00:00Z", 2), row("2026-01-01T23:59:00Z", -1),
    row("2026-01-03T12:00:00Z", null), row("2026-01-04T12:00:00Z", -1), row("2026-01-10T12:00:00Z", 2)]
    .map((item) => ({ ...item, tpPct: 2, slPct: 1, tpPrice: 98, slPrice: 101 }));
  const days = buildDailyExpectancy(rows);
  const originUnix = unix("2026-01-01T00:00:00Z");
  const model = buildDailyExpectancyModel(days, originUnix);
  expect(model.originUnix).toBe(1767225600);
  expect(model.points.map((item) => [item.day, item.dayStartUnix / 86400, item.x, item.riskExpectancy, item.closedCount, item.tpCount, item.slCount])).toEqual([
    ["2026-01-01", 20454, 0, 1, 3, 2, 1], ["2026-01-03", 20456, 2, null, 0, 0, 0],
    ["2026-01-04", 20457, 3, -1, 1, 0, 1], ["2026-01-10", 20463, 9, 2, 1, 1, 0],
  ]);
  expect(model.points.map((item) => item.computedCount)).toEqual([3, 1, 1, 1]);
  expect(model.segments.map((segment) => segment.map((item) => item.x))).toEqual([[0], [3, 9]]);
  // Selected-pair mean remains trade-weighted (0.8 R), unlike the mean of daily means (2/3 R).
  const closedReturns = rows.map((item) => item.riskReturn).filter((value): value is number => value !== null);
  expect(closedReturns.reduce((sum, value) => sum + value, 0) / closedReturns.length).toBe(0.8);
  expect(100 * days.reduce((sum, item) => sum + item.tpCount, 0) / closedReturns.length).toBe(60);
});

it("uses the displayed period origin, not the first finite day, even with leading null buckets", () => {
  const model = buildDailyExpectancyModel([point("2026-01-03", null), point("2026-01-04", -1), point("2026-01-10", 2), point("2026-01-12", null)], unix("2025-12-31T15:30:00Z"));
  expect(model.originUnix).toBe(unix("2025-12-31T00:00:00Z"));
  expect(model.points.map((item) => item.x)).toEqual([3, 4, 10, 12]);
  expect(model.segments.map((segment) => segment.map((item) => item.x))).toEqual([[4, 10]]);
});

it("keeps actual gaps between represented days and never fabricates zero-filled days", () => {
  const model = buildDailyExpectancyModel([point("2026-01-01", 1), point("2026-01-02", 2), point("2026-01-10", 3)], unix("2026-01-01T00:00:00Z"));
  expect(model.points.map((item) => item.x)).toEqual([0, 1, 9]);
  expect(model.points).toHaveLength(3); expect(model.points.some((item) => item.riskExpectancy === 0)).toBe(false);
});

it.each([[], [point("2026-01-01", null)], [point("2026-01-01", 2)], [point("2026-01-01", null), point("2026-01-10", 2)]].map((points) => ({ points })))("has no artificial trend for empty/null/single-finite data: $points", ({ points }) => {
  const model = buildDailyExpectancyModel(points, unix("2026-01-01T00:00:00Z"));
  expect(model.segments.flat()).toHaveLength(points.filter((p) => p.riskExpectancy !== null).length);
  expect(model).not.toHaveProperty("trend"); // No independent browser regression.
});

it("treats true zero as a valid green-side point while ignoring nonfinite/null means and breaking segments", () => {
  const model = buildDailyExpectancyModel([point("2026-01-01", 0), point("2026-01-02", NaN), point("2026-01-03", Infinity), point("2026-01-05", 0)], unix("2026-01-01T00:00:00Z"));
  expect(model.segments.map((segment) => segment.map((item) => item.x))).toEqual([[0], [4]]);
  expect(model.segments.flat().map((p) => p.riskExpectancy)).toEqual([0, 0]);
});

it("groups UTC midnight boundaries by signal rather than exit, with no DST-dependent day duration", () => {
  const days = buildDailyExpectancy([row("2026-03-08T23:59:00Z", 2), row("2026-03-09T00:00:00Z", -1), row("2026-03-09T00:00:01Z", 1)]);
  expect(days.map((item) => [item.day, item.riskExpectancy, item.closedCount])).toEqual([["2026-03-08", 2, 1], ["2026-03-09", 0, 2]]);
  expect(days[1]!.dayStartUnix - days[0]!.dayStartUnix).toBe(86400);
});

it("retains unclosed/no-data buckets but excludes them and malformed nonfinite returns from closed means/counts", () => {
  const days = buildDailyExpectancy([row("2026-01-01T12:00:00Z", 1), row("2026-01-01T12:01:00Z", NaN), row("2026-01-01T12:02:00Z", Infinity),
    row("2026-01-02T12:00:00Z", null, "no_data"), row("2026-01-02T12:01:00Z", 2, "unclosed")]);
  expect(days[0]).toMatchObject({ riskExpectancy: 1, closedCount: 1, computedCount: 3, cachedCount: 3 });
  expect(days[1]).toMatchObject({ riskExpectancy: null, closedCount: 0, tpCount: 0, slCount: 0, computedCount: 2 });
});

it("rejects invalid timestamps, duplicate days and reversed inputs rather than fitting corrupted coordinates", () => {
  expect(() => utcDayStart(NaN)).toThrow(RangeError);
  expect(() => buildDailyExpectancyModel([point("2026-01-01", 1), point("2026-01-01", 2)], 0)).toThrow("unique ascending");
  expect(() => buildDailyExpectancyModel([point("2026-01-10", 1), point("2026-01-01", 2)], 0)).toThrow("unique ascending");
});
