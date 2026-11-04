import { describe, expect, it } from "vitest";
import {
  DEFAULT_SPIKE_PARAMS,
  PriceVolumeSpikeDetector,
  SPIKE_ALGORITHM_VERSION,
  type DetectorBar,
} from "../src/detector/price-volume-spike.js";

const MINUTE = 60;
const DAY_MINUTES = 1440;
const LONG_MINUTES = 30 * DAY_MINUTES; // 43200
const SHORT_MINUTES = 7 * DAY_MINUTES; // 10080

interface BarSpec {
  close: string;
  volume: string;
}

function series(specs: BarSpec[], startOffsetMinutes = 0): DetectorBar[] {
  return specs.map((spec, index) => ({
    openTimeUnix: (startOffsetMinutes + index) * MINUTE,
    close: spec.close,
    quoteTurnover: spec.volume,
  }));
}

function flat(count: number, close: string, volume: string): BarSpec[] {
  return Array.from({ length: count }, () => ({ close, volume }));
}

/** Creates bars with a gap of `gapMinutes` after `before` bars, then `after` more bars. */
function gappedSeries(
  before: number,
  gapMinutes: number,
  after: number,
  close = "100",
  volume = "100",
): DetectorBar[] {
  const bars: DetectorBar[] = [];
  for (let i = 0; i < before; i += 1) {
    bars.push({ openTimeUnix: i * MINUTE, close, quoteTurnover: volume });
  }
  const afterStart = before + gapMinutes;
  for (let i = 0; i < after; i += 1) {
    bars.push({
      openTimeUnix: (afterStart + i) * MINUTE,
      close,
      quoteTurnover: volume,
    });
  }
  return bars;
}

function collectSetups(bars: DetectorBar[], params = DEFAULT_SPIKE_PARAMS) {
  const detector = new PriceVolumeSpikeDetector(params);
  const setups = [];
  for (const bar of bars) {
    const result = detector.push(bar);
    if (result.setup !== undefined) {
      setups.push(result.setup);
    }
  }
  return setups;
}

describe("PriceVolumeSpikeDetector", () => {
  it("uses exact fractional thresholds without decimal-scale inflation", () => {
    const params = { ...DEFAULT_SPIKE_PARAMS, priceGrowthPct: "12.5", volumeMultiplier: "2.5" };
    expect(collectSetups(series([...flat(LONG_MINUTES - 5, "100", "100"), ...flat(5, "112.5", "251")]), params)).toHaveLength(1);
    expect(collectSetups(series([...flat(LONG_MINUTES - 5, "100", "100"), ...flat(5, "112.49", "251")]), params)).toHaveLength(0);
  });

  it("keeps equal baseline rolling sums accurate after eviction", () => {
    const params = { ...DEFAULT_SPIKE_PARAMS, baselineLongDays: 1, baselineShortDays: 1 };
    const bars = series([...flat(4000, "100", "100"), ...flat(5, "112", "310")]);
    expect(collectSetups(bars, params)).toHaveLength(1);
  });
  it("emits a setup on exactly 12% growth with strictly higher volume", () => {
    const bars = series([
      ...flat(LONG_MINUTES - 5, "100", "100"),
      ...flat(5, "112", "301"),
    ]);
    const setups = collectSetups(bars);
    expect(setups.length).toBeGreaterThanOrEqual(1);
    expect(setups[0]?.closePrice).toBe("112");
    expect(setups[0]?.closeTimeUnix).toBe((LONG_MINUTES - 1) * MINUTE + MINUTE);
    expect(setups[0]?.algorithmId).toBe("price_volume_spike");
    expect(setups[0]?.algorithmVersion).toBe(SPIKE_ALGORITHM_VERSION);
  });

  it("does not emit a setup when volume is exactly 3x the baseline average", () => {
    const bars = series([
      ...flat(LONG_MINUTES - 5, "100", "100"),
      ...flat(5, "112", "300"),
    ]);
    const setups = collectSetups(bars);
    expect(setups).toHaveLength(0);
  });

  it("emits via the 7-day baseline when both baselines are complete", () => {
    const specs: BarSpec[] = [];
    specs.push(...flat(LONG_MINUTES - SHORT_MINUTES, "100", "1000"));
    specs.push(...flat(SHORT_MINUTES - 5, "100", "100"));
    specs.push(...flat(5, "112", "301"));
    const setups = collectSetups(series(specs));
    expect(setups.length).toBeGreaterThanOrEqual(1);
  });

  it("does not emit a setup below the growth threshold", () => {
    const bars = series([
      ...flat(LONG_MINUTES - 5, "100", "100"),
      ...flat(5, "111.99", "1000"),
    ]);
    const setups = collectSetups(bars);
    expect(setups).toHaveLength(0);
  });

  it("keeps minutes unevaluated until 43200 consecutive minutes", () => {
    const detector = new PriceVolumeSpikeDetector();
    const warm = series(flat(LONG_MINUTES - 1, "100", "100"));
    for (const bar of warm) {
      const result = detector.push(bar);
      expect(result.status).toBe("warmup");
    }
    const firstEvaluated = detector.push({
      openTimeUnix: (LONG_MINUTES - 1) * MINUTE,
      close: "112",
      quoteTurnover: "301",
    });
    expect(firstEvaluated.status).toBe("evaluated");
  });

  it("respects the five-minute cooldown for a sustained condition", () => {
    const spikeCloses = [
      ...Array<string>(5).fill("112"),
      ...Array<string>(5).fill("125.44"),
      "140.4928",
      "140.4928",
    ];
    const specs: BarSpec[] = [
      ...flat(LONG_MINUTES, "100", "100"),
      ...spikeCloses.map((close) => ({ close, volume: "301" })),
    ];
    const setups = collectSetups(series(specs));
    expect(setups).toHaveLength(2);
    expect(setups[0]?.openTimeUnix).toBe((LONG_MINUTES + 4) * MINUTE);
    expect(setups[1]?.openTimeUnix).toBe((LONG_MINUTES + 9) * MINUTE);
    expect(setups[1]!.openTimeUnix - setups[0]!.openTimeUnix).toBe(5 * MINUTE);
  });

  it("applies changed parameters", () => {
    const params = {
      ...DEFAULT_SPIKE_PARAMS,
      priceGrowthPct: "6",
    };
    const bars = series([
      ...flat(LONG_MINUTES - 5, "100", "100"),
      ...flat(5, "106", "301"),
    ]);
    const setups = collectSetups(bars, params);
    expect(setups).toHaveLength(1);
    expect(setups[0]?.params.priceGrowthPct).toBe("6");
  });
});

describe("PriceVolumeSpikeDetector gap tolerance", () => {
  it("tolerates a single 1-minute gap after full warmup and still evaluates", () => {
    // Warmup fills, then a 1-minute gap, then enough bars to re-fill the
    // requiredRecentMinutes window. The bar after the gap zone should be
    // evaluated (status = evaluated).
    const bars = gappedSeries(
      LONG_MINUTES - 30, // before gap: warmup minus 30 minutes
      1, // 1-minute gap
      30, // 30 bars after gap (enough for requiredRecentMinutes=10)
    );
    const detector = new PriceVolumeSpikeDetector();
    let lastStatus = "";
    for (const bar of bars) {
      const result = detector.push(bar);
      lastStatus = result.status;
    }
    // After 30 post-gap bars, the window is full again and evaluation resumes
    expect(lastStatus).toBe("evaluated");
  });

  it("tolerates a 5-minute gap (at the limit) without resetting", () => {
    const bars = gappedSeries(
      LONG_MINUTES - 40,
      5, // exactly at maxConsecutiveGapMinutes
      40,
    );
    const detector = new PriceVolumeSpikeDetector();
    let lastStatus = "";
    for (const bar of bars) {
      const result = detector.push(bar);
      lastStatus = result.status;
    }
    expect(lastStatus).toBe("evaluated");
  });

  it("resets when a gap exceeds maxConsecutiveGapMinutes", () => {
    const bars = gappedSeries(
      LONG_MINUTES - 10,
      6, // exceeds maxConsecutiveGapMinutes=5
      20,
    );
    const detector = new PriceVolumeSpikeDetector();
    let lastResult;
    for (const bar of bars) {
      lastResult = detector.push(bar);
    }
    // After reset, we only have ~20 entries, so still in warmup
    expect(lastResult?.status).toBe("warmup");
    expect(lastResult?.continuousMinutes).toBeLessThan(LONG_MINUTES);
  });

  it("does not evaluate a minute within the requiredRecentMinutes gap-free zone", () => {
    // Create a full window, then a small gap, then a bar immediately after.
    // The bar right after the gap should NOT be evaluated because the last
    // 10 minutes include the gap-filler nulls.
    const bars = gappedSeries(
      LONG_MINUTES - 2,
      2,
      1, // only 1 bar after the gap: within requiredRecentMinutes=10 zone
    );
    const detector = new PriceVolumeSpikeDetector();
    let lastResult;
    for (const bar of bars) {
      lastResult = detector.push(bar);
    }
    // A full window is insufficient while recent candles are missing.
    expect(lastResult?.status).toBe("warmup");
    expect(lastResult?.setup).toBeUndefined();
  });

  it("evaluates normally after gaps have moved out of the recent zone", () => {
    // Full warmup with gaps, then enough clean bars to clear requiredRecentMinutes
    const bars = [
      ...gappedSeries(LONG_MINUTES - 60, 3, 30, "100", "100"),
      // Add 30 more clean bars with a spike at the end
      ...series(
        [
          ...flat(25, "100", "100"),
          ...flat(5, "112", "301"),
        ],
        LONG_MINUTES - 60 + 3 + 30,
      ),
    ];
    const setups = collectSetups(bars);
    expect(setups.length).toBeGreaterThanOrEqual(1);
    expect(setups[0]?.closePrice).toBe("112");
  });

  it("resets when total missing exceeds maxGapPercent of the window", () => {
    // 1% of 43200 = 432. Create many small gaps that together exceed this.
    // With gapMinutes=5 and 87 gaps we get 435 missing > 432.
    const bars: DetectorBar[] = [];
    let time = 0;
    let pushed = 0;
    const totalNeeded = LONG_MINUTES + 500;
    while (pushed < totalNeeded) {
      // Push 5 real bars
      for (let i = 0; i < 5 && pushed < totalNeeded; i += 1) {
        bars.push({ openTimeUnix: time, close: "100", quoteTurnover: "100" });
        time += MINUTE;
        pushed += 1;
      }
      // Create a 5-minute gap
      if (pushed < totalNeeded) {
        time += 5 * MINUTE;
      }
    }
    const detector = new PriceVolumeSpikeDetector();
    let lastResult;
    for (const bar of bars) {
      lastResult = detector.push(bar);
    }
    // With ~87 gaps × 5 min = ~435 missing out of window, detector resets
    // and starts warmup again
    expect(lastResult?.status).toBe("warmup");
  });

  it("includes gap parameters in the emitted setup", () => {
    const bars = series([
      ...flat(LONG_MINUTES - 5, "100", "100"),
      ...flat(5, "112", "301"),
    ]);
    const setups = collectSetups(bars);
    expect(setups[0]?.params.maxGapPercent).toBe("1");
    expect(setups[0]?.params.maxConsecutiveGapMinutes).toBe(5);
    expect(setups[0]?.params.requiredRecentMinutes).toBe(10);
  });

  it("allows customizing gap tolerance parameters", () => {
    const params = {
      ...DEFAULT_SPIKE_PARAMS,
      maxGapPercent: "0",
      maxConsecutiveGapMinutes: 0,
      requiredRecentMinutes: 0,
    };
    // With zero tolerance, any gap resets (same as old behavior)
    const bars = gappedSeries(LONG_MINUTES - 10, 1, 20);
    const detector = new PriceVolumeSpikeDetector(params);
    let lastResult;
    for (const bar of bars) {
      lastResult = detector.push(bar);
    }
    expect(lastResult?.status).toBe("warmup");
  });
});
