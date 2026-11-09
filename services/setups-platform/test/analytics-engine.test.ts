import { describe, expect, it } from "vitest";
import {
  aggregateLimitOutcomes,
  evaluateLimit,
  filterByMonth,
  iterSetupClusters,
  monthKeyOf,
  passesPriceChange,
  resolveEntry,
  riskReturn,
  selectByMaturity,
  setupMonths,
  type AnalyticsSetup,
  type OhlcCandle,
} from "../src/analytics/engine.js";

const MINUTE = 60;
/** 2024-06-01T00:00:00Z, exact month boundary. */
const T0 = Math.floor(Date.UTC(2024, 5, 1) / 1000);

function makeSetup(symbol: string, closeTimeUnix: number, closePrice = "100"): AnalyticsSetup {
  return {
    exchangeCode: "bybit",
    category: "linear",
    symbol,
    openTimeUnix: closeTimeUnix - MINUTE,
    closeTimeUnix,
    closePrice,
  };
}

function candle(openTimeUnix: number, high: number, low: number, close: number): OhlcCandle {
  return { openTimeUnix, open: close, high, low, close };
}

/** Flat candles at `price` covering [fromUnix, fromUnix + count minutes). */
function flatCandles(fromUnix: number, count: number, price: number): OhlcCandle[] {
  return Array.from({ length: count }, (_, index) => candle(fromUnix + index * MINUTE, price, price, price));
}

describe("analytics engine", () => {
  it("computes UTC month keys and months list", () => {
    expect(monthKeyOf(T0)).toBe("2024-06");
    expect(monthKeyOf(Math.floor(Date.UTC(2024, 6, 1) / 1000) - 1)).toBe("2024-06");
    expect(monthKeyOf(Math.floor(Date.UTC(2025, 0, 31, 23, 59) / 1000))).toBe("2025-01");
    const setups = [
      makeSetup("AAA", Math.floor(Date.UTC(2024, 5, 15) / 1000)),
      makeSetup("BBB", Math.floor(Date.UTC(2024, 6, 2) / 1000)),
      makeSetup("AAA", Math.floor(Date.UTC(2024, 6, 3) / 1000)),
    ];
    expect(setupMonths(setups)).toEqual(["2024-06", "2024-07"]);
  });

  it("filters setups by UTC month without touching the horizon", () => {
    const june = makeSetup("AAA", Math.floor(Date.UTC(2024, 5, 30, 23) / 1000));
    const july = makeSetup("AAA", Math.floor(Date.UTC(2024, 6, 1) / 1000));
    const setups = [june, july];
    expect(filterByMonth(setups, "2024-06")).toEqual([june]);
    expect(filterByMonth(setups, "2024-07")).toEqual([july]);
  });

  it("clusters same-instrument setups while gaps stay within 8 hours", () => {
    const within = 8 * 3600;
    const setups = [
      makeSetup("AAA", T0),
      makeSetup("AAA", T0 + within), // exactly 8h gap: same cluster
      makeSetup("AAA", T0 + within + 60 + within + 1), // > 8h after previous: new cluster
      makeSetup("BBB", T0 + 60), // other instrument: own cluster
    ];
    const clusters = iterSetupClusters(setups);
    expect(clusters).toHaveLength(3);
    const aaaClusters = clusters.filter((cluster) => cluster[0]?.symbol === "AAA");
    expect(aaaClusters.map((cluster) => cluster.length)).toEqual([2, 1]);
  });

  it("honours the custom cluster gap parameter", () => {
    const setups = [
      makeSetup("AAA", T0),
      makeSetup("AAA", T0 + 2 * 3600), // 2h after the first
      makeSetup("AAA", T0 + 2 * 3600 + 5 * 3600), // 5h after the previous
    ];
    expect(iterSetupClusters(setups)).toHaveLength(1); // default 8h: single cluster
    expect(iterSetupClusters(setups, 3 * 3600)).toHaveLength(2); // 3h gap splits the chain
    const selection = selectByMaturity(setups, 0.5, 3 * 3600);
    expect(selection.totalClustersCount).toBe(2);
    expect(selection.rows).toHaveLength(2);
  });

  it("selects the setup nearest to the maturity point, ties to the earlier one", () => {
    const cluster = [
      makeSetup("AAA", T0),
      makeSetup("AAA", T0 + 3600),
      makeSetup("AAA", T0 + 7200),
    ];
    const mid = selectByMaturity(cluster, 0.5);
    expect(mid.rows).toHaveLength(1);
    expect(mid.rows[0]?.closeTimeUnix).toBe(T0 + 3600);
    expect(mid.avgSelectionOrdinal).toBe(2);
    expect(mid.avgSelectionDelayMinutes).toBe(60);

    const start = selectByMaturity(cluster, 0);
    expect(start.rows[0]?.closeTimeUnix).toBe(T0);

    const end = selectByMaturity(cluster, 1);
    expect(end.rows[0]?.closeTimeUnix).toBe(T0 + 7200);
    expect(end.avgSelectionOrdinal).toBe(3);

    // Equidistant between first and middle: earlier setup wins.
    const tie = selectByMaturity([cluster[0]!, cluster[1]!], 0.25);
    expect(tie.rows[0]?.closeTimeUnix).toBe(T0);
    expect(tie.totalClustersCount).toBe(1);
  });

  it("resolves entry as close of the candle N minutes after the signal candle", () => {
    const setup = makeSetup("AAA", T0 + MINUTE, "100"); // signal candle opens at T0
    const candles = [
      candle(T0, 100, 100, 100),
      candle(T0 + MINUTE, 110, 90, 105),
      candle(T0 + 2 * MINUTE, 120, 80, 115),
    ];
    expect(resolveEntry(candles, setup, 0)?.entryPrice).toBe(100);
    expect(resolveEntry(candles, setup, 1)?.entryPrice).toBe(105);
    expect(resolveEntry(candles, setup, 2)?.entryPrice).toBe(115);
    expect(resolveEntry(candles, setup, 5)).toBeNull();
  });

  it("detects short TP/SL touches with SL priority in one candle", () => {
    const setup = makeSetup("AAA", T0 + MINUTE, "100");
    const entryClose = 100;
    const candles = [
      candle(T0, entryClose, entryClose, entryClose), // signal + entry candle (shift 0)
      candle(T0 + MINUTE, 101, 99, 100), // touches neither TP=96 nor SL=104
      candle(T0 + 2 * MINUTE, 105, 95, 100), // touches both TP and SL -> SL wins
    ];
    const evaluation = evaluateLimit({
      candles, setup, direction: "short", tpPct: 4, slPct: 4, timeShiftMinutes: 0, horizonMinutes: 60,
    });
    expect(evaluation.status).toBe("sl");
    expect(evaluation.entryPrice).toBe(100);
    expect(evaluation.slPrice).toBe(104);
    expect(evaluation.tpPrice).toBeNull();
    expect(evaluation.movePct).toBe(-4);
    expect(evaluation.closeMinutes).toBe(1);
  });

  it("reports TP for short when only the TP bound is touched", () => {
    const setup = makeSetup("AAA", T0 + MINUTE, "100");
    const candles = [
      candle(T0, 100, 100, 100),
      candle(T0 + MINUTE, 100, 100, 100),
      candle(T0 + 2 * MINUTE, 100, 95.9, 96),
    ];
    const evaluation = evaluateLimit({
      candles, setup, direction: "short", tpPct: 4, slPct: 4, timeShiftMinutes: 0, horizonMinutes: 60,
    });
    expect(evaluation.status).toBe("tp");
    expect(evaluation.tpPrice).toBeCloseTo(96);
    expect(evaluation.movePct).toBe(4);
  });

  it("ignores the entry candle itself and candles beyond the horizon", () => {
    const setup = makeSetup("AAA", T0 + MINUTE, "100"); // entry candle opens at T0
    const spikyEntry = candle(T0, 200, 50, 100); // entry candle touches everything, but entry is at its close
    const beyondHorizon = candle(T0 + MINUTE + 10 * MINUTE, 200, 1, 100); // 10 minutes after entry, horizon 9
    const candles = [spikyEntry, beyondHorizon];
    const evaluation = evaluateLimit({
      candles, setup, direction: "short", tpPct: 4, slPct: 4, timeShiftMinutes: 0, horizonMinutes: 9,
    });
    expect(evaluation.status).toBe("unclosed");
  });

  it("mirrors TP/SL for long direction", () => {
    const setup = makeSetup("AAA", T0 + MINUTE, "100");
    const candles = [
      candle(T0, 100, 100, 100),
      candle(T0 + MINUTE, 100, 100, 100),
      candle(T0 + 2 * MINUTE, 103.5, 99, 100), // TP=104 not touched (high<104), SL=96 not touched -> nothing
      candle(T0 + 3 * MINUTE, 104.5, 99, 100), // TP=104 touched
    ];
    const evaluation = evaluateLimit({
      candles, setup, direction: "long", tpPct: 4, slPct: 4, timeShiftMinutes: 0, horizonMinutes: 60,
    });
    expect(evaluation.status).toBe("tp");
    expect(evaluation.tpPrice).toBeCloseTo(104);

    const both = evaluateLimit({
      candles: [candles[0]!, candles[1]!, candle(T0 + 2 * MINUTE, 105, 95, 100)],
      setup, direction: "long", tpPct: 4, slPct: 4, timeShiftMinutes: 0, horizonMinutes: 60,
    });
    expect(both.status).toBe("sl");
  });

  it("returns no_data when the entry candle is missing", () => {
    const setup = makeSetup("AAA", T0 + 2 * MINUTE, "100"); // entry candle opens at T0+60
    const evaluation = evaluateLimit({
      candles: [candle(T0, 100, 100, 100)], setup, direction: "short",
      tpPct: 4, slPct: 4, timeShiftMinutes: 0, horizonMinutes: 60,
    });
    expect(evaluation.status).toBe("no_data");
  });

  it("applies the price-change gate in both directions", () => {
    // short: shifted must be >= setup close * (1 + pct/100)
    expect(passesPriceChange(100, 100, "short", 0)).toBe(true);
    expect(passesPriceChange(100, 99.9, "short", 0)).toBe(false);
    expect(passesPriceChange(100, 105, "short", 4)).toBe(true);
    expect(passesPriceChange(100, 103.9, "short", 4)).toBe(false);
    expect(passesPriceChange(100, 98, "short", -2)).toBe(true);
    // long: shifted must be <= setup close * (1 - pct/100)
    expect(passesPriceChange(100, 96, "long", 4)).toBe(true);
    expect(passesPriceChange(100, 96.1, "long", 4)).toBe(false);
    expect(passesPriceChange(100, 100, "long", 0)).toBe(true);
    expect(passesPriceChange(100, 101, "long", 0)).toBe(false);
  });

  it("computes risk return per row", () => {
    expect(riskReturn("tp", 4, 2)).toBe(2);
    expect(riskReturn("sl", 4, 2)).toBe(-1);
    expect(riskReturn("unclosed", 4, 2)).toBeNull();
    expect(riskReturn("no_data", 4, 2)).toBeNull();
  });

  it("aggregates only closed trades and derives risk expectancy", () => {
    const closed = [
      { status: "tp", movePct: 4, closeMinutes: 10 } as const,
      { status: "sl", movePct: -4, closeMinutes: 30 } as const,
    ];
    const extras = [
      { status: "unclosed", movePct: null, closeMinutes: null } as const,
      { status: "no_data", movePct: null, closeMinutes: null } as const,
      { status: "price_change_filtered", movePct: null, closeMinutes: null } as const,
    ];
    const aggregate = aggregateLimitOutcomes([...closed, ...extras] as never, 4);
    expect(aggregate.closedCount).toBe(2);
    expect(aggregate.tpCount).toBe(1);
    expect(aggregate.slCount).toBe(1);
    expect(aggregate.unclosedCount).toBe(1);
    expect(aggregate.noDataCount).toBe(1);
    expect(aggregate.avgMovePct).toBe(0);
    expect(aggregate.winRatePct).toBe(50);
    expect(aggregate.avgCloseMinutes).toBe(20);
    expect(aggregate.medianCloseMinutes).toBe(20);
    expect(aggregate.riskExpectancy).toBe(0);

    const empty = aggregateLimitOutcomes([], 4);
    expect(empty.avgMovePct).toBeNull();
    expect(empty.riskExpectancy).toBeNull();
    expect(empty.winRatePct).toBeNull();
  });

  it("keeps short entries flat until a spike candle appears", () => {
    const setup = makeSetup("AAA", T0 + MINUTE, "100");
    const candles = [...flatCandles(T0, 5, 100)];
    const shifted = evaluateLimit({
      candles, setup, direction: "short", tpPct: 1, slPct: 1, timeShiftMinutes: 3, horizonMinutes: 1,
    });
    // Entry at candle T0+3min close (100), horizon 1 minute scans only T0+4min.
    expect(shifted.status).toBe("unclosed");
    expect(shifted.entryPrice).toBe(100);
  });
});
