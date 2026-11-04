import { describe, expect, it } from "vitest";
import { ReplayQualityTracker } from "../src/replay/quality.js";
import { PriceVolumeSpikeDetector, DEFAULT_SPIKE_PARAMS } from "../src/detector/price-volume-spike.js";
import { replayRun } from "../src/replay/replay-runner.js";
import { ArrayCandleSource } from "../src/replay/source.js";

const evaluated = { status: "evaluated", continuousMinutes: 1440 } as const;

describe("replay quality", () => {
  it("partitions requested minutes without counting warmup input or unvisited tail as gaps", () => {
    const tracker = new ReplayQualityTracker(600, 1200, 1440);
    tracker.observe(540, evaluated);
    tracker.observe(660, { status: "warmup", continuousMinutes: 10, skipReason: "insufficient_warmup" });
    tracker.observe(780, { ...evaluated, missingMinutes: 1 });
    expect(tracker.quality).toMatchObject({ requestedMinutes: 10, evaluatedMinutes: 1,
      evaluatedWithGapsMinutes: 1, skippedMinutes: 3, unassessedMinutes: 6, state: "in_progress",
      reasonCounts: { missing_candle: 2, insufficient_warmup: 1 } });
    expect(tracker.drain()).toHaveLength(4);
    expect(tracker.drain()).toEqual([]);
    tracker.finish();
    expect(tracker.quality).toMatchObject({ state: "partial", skippedMinutes: 9, unassessedMinutes: 0,
      reasonCounts: { missing_candle: 8, insufficient_warmup: 1 } });
    expect(tracker.drain()).toEqual([{ fromUnix: 840, toUnix: 1200, reason: "missing_candle" }]);
  });

  it("handles empty data and nonaligned requested bounds on the UTC minute grid", () => {
    const tracker = new ReplayQualityTracker(601, 781, 1440);
    tracker.finish();
    expect(tracker.quality).toMatchObject({ requestedMinutes: 3, skippedMinutes: 3, state: "no_eligible_data", assessedThroughUnix: 840 });
    expect(tracker.drain()).toEqual([{ fromUnix: 660, toUnix: 840, reason: "missing_candle" }]);
  });

  it("does not mistake a missing price anchor for an evaluated non-signal", () => {
    const detector = new PriceVolumeSpikeDetector({ ...DEFAULT_SPIKE_PARAMS, baselineLongDays: 1,
      baselineShortDays: 1, windowMinutes: 5, requiredRecentMinutes: 1 });
    for (let minute = 0; minute < 1440; minute++) {
      if (minute !== 1434) detector.push({ openTimeUnix: minute * 60, close: "1", quoteTurnover: "1" });
    }
    const result = detector.push({ openTimeUnix: 1440 * 60, close: "1", quoteTurnover: "1" });
    expect(result.skipReason).toBeUndefined();
    const missing = new PriceVolumeSpikeDetector({ ...DEFAULT_SPIKE_PARAMS, baselineLongDays: 1,
      baselineShortDays: 1, windowMinutes: 5, requiredRecentMinutes: 1 });
    for (let minute = 0; minute < 1440; minute++) {
      if (minute !== 1435) missing.push({ openTimeUnix: minute * 60, close: "1", quoteTurnover: "1" });
    }
    expect(missing.push({ openTimeUnix: 1440 * 60, close: "1", quoteTurnover: "1" }))
      .toMatchObject({ status: "evaluated", skipReason: "price_reference_missing" });
  });

  it("flushes bounded intervals across repeated pages and keeps exact totals", () => {
    const tracker = new ReplayQualityTracker(0, 120000, 1440);
    for (let page = 0; page < 20; page++) {
      for (let minute = page * 100; minute < page * 100 + 100; minute++) {
        tracker.observe(minute * 60, minute % 2 ? evaluated : { status: "warmup", continuousMinutes: 1, skipReason: "recent_gap" });
      }
      expect(tracker.drain().length).toBe(100);
    }
    tracker.finish();
    expect(tracker.drain()).toEqual([]);
    expect(tracker.quality).toMatchObject({ evaluatedMinutes: 1000, skippedMinutes: 1000, unassessedMinutes: 0 });
  });

  it("preserves a cancelled prefix in synchronous replay and never finalizes the tail", async () => {
    const cancel = { cancelled: false };
    const result = await replayRun({ fromUnix: 0, toUnix: 600, warmupUnix: 0, cancel,
      source: new ArrayCandleSource(Array.from({ length: 10 }, (_, index) => ({ openTimeUnix: index * 60, close: "1", volume: "1", quoteTurnover: "1" }))),
      onCheckpoint: (time) => { if (time === 120) cancel.cancelled = true; },
    });
    expect(result.quality).toMatchObject({ state: "in_progress", skippedMinutes: 3, unassessedMinutes: 7,
      reasonCounts: { missing_candle: 0, insufficient_warmup: 3 } });
  });
});
