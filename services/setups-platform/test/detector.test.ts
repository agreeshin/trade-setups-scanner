import { describe, expect, it } from "vitest";
import {
  DEFAULT_SPIKE_PARAMS,
  PriceVolumeSpikeDetector,
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

function series(
  specs: BarSpec[],
  startOffsetMinutes = 0,
): DetectorBar[] {
  return specs.map((spec, index) => ({
    openTimeUnix: (startOffsetMinutes + index) * MINUTE,
    close: spec.close,
    quoteTurnover: spec.volume,
  }));
}

function flat(count: number, close: string, volume: string): BarSpec[] {
  return Array.from({ length: count }, () => ({ close, volume }));
}

describe("PriceVolumeSpikeDetector", () => {
  it("emits a setup on exactly 12% growth with strictly higher volume", () => {
    const bars = series([
      ...flat(LONG_MINUTES - 5, "100", "100"),
      ...flat(5, "112", "301"),
    ]);
    const detector = new PriceVolumeSpikeDetector();
    let setup = undefined;
    for (const bar of bars) {
      const result = detector.push(bar);
      if (result.setup !== undefined) {
        setup = result.setup;
      }
    }
    expect(setup).toBeDefined();
    expect(setup?.closePrice).toBe("112");
    expect(setup?.closeTimeUnix).toBe((LONG_MINUTES - 1) * MINUTE + MINUTE);
    expect(setup?.algorithmId).toBe("price_volume_spike");
    expect(setup?.algorithmVersion).toBe("1.0.0");
    expect(setup?.params).toEqual(DEFAULT_SPIKE_PARAMS);
  });

  it("does not emit a setup when volume is exactly 3x the baseline average", () => {
    const bars = series([
      ...flat(LONG_MINUTES - 5, "100", "100"),
      ...flat(5, "112", "300"),
    ]);
    const detector = new PriceVolumeSpikeDetector();
    const setups = bars
      .map((bar) => detector.push(bar))
      .filter((result) => result.setup !== undefined);
    expect(setups).toHaveLength(0);
  });

  it("emits via the 7-day baseline when both baselines are complete", () => {
    // Old heavy volume inflates the 30-day average far above the window, while
    // the 7-day baseline stays low, so only the short baseline fires.
    const specs: BarSpec[] = [];
    specs.push(...flat(LONG_MINUTES - SHORT_MINUTES, "100", "1000"));
    specs.push(...flat(SHORT_MINUTES - 5, "100", "100"));
    specs.push(...flat(5, "112", "301"));
    const detector = new PriceVolumeSpikeDetector();
    let setup = undefined;
    for (const bar of series(specs)) {
      const result = detector.push(bar);
      if (result.setup !== undefined) {
        setup = result.setup;
      }
    }
    expect(setup).toBeDefined();
  });

  it("does not emit a setup below the growth threshold", () => {
    const bars = series([
      ...flat(LONG_MINUTES - 5, "100", "100"),
      ...flat(5, "111.99", "1000"),
    ]);
    const detector = new PriceVolumeSpikeDetector();
    const setups = bars
      .map((bar) => detector.push(bar))
      .filter((result) => result.setup !== undefined);
    expect(setups).toHaveLength(0);
  });

  it("keeps minutes unevaluated until 43200 consecutive minutes", () => {
    const detector = new PriceVolumeSpikeDetector();
    const warm = series([
      ...flat(LONG_MINUTES - 1, "100", "100"),
    ]);
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

  it("treats 29 days of history as insufficient", () => {
    const detector = new PriceVolumeSpikeDetector();
    const bars = series(flat(29 * DAY_MINUTES, "100", "100"));
    let last;
    for (const bar of bars) {
      last = detector.push(bar);
    }
    expect(last?.status).toBe("warmup");
    expect(last?.continuousMinutes).toBe(29 * DAY_MINUTES);
  });

  it("respects the five-minute cooldown for a sustained condition", () => {
    // Exact 12% steps every five minutes: 112 = 100 * 1.12,
    // 125.44 = 112 * 1.12, 140.4928 = 125.44 * 1.12.
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
    const detector = new PriceVolumeSpikeDetector();
    const setups = series(specs)
      .map((bar) => detector.push(bar))
      .filter((result) => result.setup !== undefined)
      .map((result) => result.setup!);
    expect(setups).toHaveLength(2);
    expect(setups[0]?.openTimeUnix).toBe((LONG_MINUTES + 4) * MINUTE);
    expect(setups[1]?.openTimeUnix).toBe((LONG_MINUTES + 9) * MINUTE);
    expect(setups[1]!.openTimeUnix - setups[0]!.openTimeUnix).toBe(5 * MINUTE);
  });

  it("restarts warmup after a gap", () => {
    const detector = new PriceVolumeSpikeDetector();
    const bars = series(flat(LONG_MINUTES, "100", "100"));
    for (const bar of bars) {
      detector.push(bar);
    }
    const afterGap = detector.push({
      openTimeUnix: LONG_MINUTES * MINUTE + MINUTE, // one minute skipped
      close: "112",
      quoteTurnover: "1000",
    });
    expect(afterGap.status).toBe("warmup");
    expect(afterGap.continuousMinutes).toBe(1);
  });

  it("applies changed parameters", () => {
    const params = {
      ...DEFAULT_SPIKE_PARAMS,
      priceGrowthPct: "6",
      baselineLongDays: 30,
    };
    const detector = new PriceVolumeSpikeDetector(params);
    const bars = series([
      ...flat(LONG_MINUTES - 5, "100", "100"),
      ...flat(5, "106", "301"),
    ]);
    const setups = bars
      .map((bar) => detector.push(bar))
      .filter((result) => result.setup !== undefined);
    expect(setups).toHaveLength(1);
    expect(setups[0]?.setup?.params.priceGrowthPct).toBe("6");
  });
});
