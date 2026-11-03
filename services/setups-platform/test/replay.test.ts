import { describe, expect, it } from "vitest";
import { PriceVolumeSpikeDetector, type SpikeSetup } from "../src/detector/price-volume-spike.js";
import { ArrayCandleSource, type CandleBarDto } from "../src/replay/source.js";
import { replayRun, type CancelToken } from "../src/replay/replay-runner.js";
import { runReplayInWorker } from "../src/replay/run-in-worker.js";

const MINUTE = 60;
const LONG_MINUTES = 30 * 1440;

interface BarSpec {
  close: string;
  volume: string;
}

function buildBars(specs: BarSpec[]): CandleBarDto[] {
  return specs.map((spec, index) => ({
    openTimeUnix: index * MINUTE,
    close: spec.close,
    volume: spec.volume,
    quoteTurnover: spec.volume,
  }));
}

function flat(count: number, close: string, volume: string): BarSpec[] {
  return Array.from({ length: count }, () => ({ close, volume }));
}

function spikeScenario(): CandleBarDto[] {
  return buildBars([
    ...flat(LONG_MINUTES, "100", "100"),
    ...flat(5, "112", "301"),
    ...flat(5, "100", "100"),
    ...flat(5, "112", "301"),
  ]);
}

describe("replay runner", () => {
  it("produces the same setups as live bar-by-bar feeding", async () => {
    const bars = spikeScenario();
    const fromUnix = LONG_MINUTES * MINUTE;
    const toUnix = bars[bars.length - 1]!.openTimeUnix + MINUTE;

    const replay = await replayRun({
      source: new ArrayCandleSource(bars),
      fromUnix,
      toUnix,
    });

    const liveDetector = new PriceVolumeSpikeDetector();
    const liveSetups: SpikeSetup[] = [];
    for (const bar of bars) {
      const result = liveDetector.push({
        openTimeUnix: bar.openTimeUnix,
        close: bar.close,
        quoteTurnover: bar.quoteTurnover,
      });
      if (result.setup !== undefined && bar.openTimeUnix >= fromUnix) {
        liveSetups.push(result.setup);
      }
    }

    expect(replay.setups.map((s) => s.openTimeUnix)).toEqual(
      liveSetups.map((s) => s.openTimeUnix),
    );
    expect(replay.setups.map((s) => s.closePrice)).toEqual(
      liveSetups.map((s) => s.closePrice),
    );
    expect(replay.setups.length).toBeGreaterThan(0);
    expect(replay.cancelled).toBe(false);
    expect(replay.processedMinutes + replay.warmupMinutes).toBe(bars.length);
  });

  it("reports warmup minutes for a gap-truncated history", async () => {
    const head = buildBars([...flat(100, "100", "100")]);
    const tail = buildBars([
      ...flat(LONG_MINUTES, "100", "100"),
      ...flat(5, "112", "301"),
    ]).map((bar) => ({ ...bar, openTimeUnix: bar.openTimeUnix + 101 * MINUTE }));
    const bars = [...head, ...tail];
    const result = await replayRun({
      source: new ArrayCandleSource(bars),
      fromUnix: 0,
      toUnix: bars[bars.length - 1]!.openTimeUnix + MINUTE,
    });
    expect(result.warmupMinutes).toBe(100 + 43199);
    expect(result.processedMinutes).toBe(6);
    expect(result.setups.length).toBeGreaterThan(0);
  });

  it("supports cooperative cancellation with a checkpoint", async () => {
    const bars = spikeScenario();
    const fromUnix = LONG_MINUTES * MINUTE;
    const cancel: CancelToken = { cancelled: false };
    let checkpoint: number | null = null;
    const result = await replayRun({
      source: new ArrayCandleSource(bars),
      fromUnix,
      toUnix: bars[bars.length - 1]!.openTimeUnix + MINUTE,
      cancel,
      onCheckpoint: (lastProcessed) => {
        checkpoint = lastProcessed;
        if (lastProcessed >= fromUnix + 2 * MINUTE) {
          cancel.cancelled = true;
        }
      },
    });
    expect(result.cancelled).toBe(true);
    expect(result.lastProcessedUnix).not.toBe(
      bars[bars.length - 1]!.openTimeUnix,
    );
    expect(checkpoint).not.toBeNull();
  });

  it("resumes deterministically after a checkpoint with re-warmup", async () => {
    const bars = spikeScenario();
    const fromUnix = LONG_MINUTES * MINUTE;
    const toUnix = bars[bars.length - 1]!.openTimeUnix + MINUTE;

    const cancel: CancelToken = { cancelled: false };
    const first = await replayRun({
      source: new ArrayCandleSource(bars),
      fromUnix,
      toUnix,
      cancel,
      onCheckpoint: (last) => {
        if (last >= fromUnix) {
          cancel.cancelled = true;
        }
      },
    });
    expect(first.cancelled).toBe(true);
    expect(first.lastProcessedUnix).not.toBeNull();

    const resumeFrom = (first.lastProcessedUnix ?? fromUnix) - (LONG_MINUTES - 1) * MINUTE;
    const second = await replayRun({
      source: new ArrayCandleSource(bars),
      fromUnix,
      toUnix,
      warmupUnix: resumeFrom,
    });

    const reference = await replayRun({
      source: new ArrayCandleSource(bars),
      fromUnix,
      toUnix,
    });
    expect(second.setups.map((s) => s.openTimeUnix)).toEqual(
      reference.setups.map((s) => s.openTimeUnix),
    );
  });

  it("runs the same replay inside a worker thread", async () => {
    const bars = spikeScenario();
    const fromUnix = LONG_MINUTES * MINUTE;
    const toUnix = bars[bars.length - 1]!.openTimeUnix + MINUTE;
    const inWorker = await runReplayInWorker({ bars, fromUnix, toUnix });
    const inline = await replayRun({
      source: new ArrayCandleSource(bars),
      fromUnix,
      toUnix,
    });
    expect(inWorker.setups.map((s) => s.openTimeUnix)).toEqual(
      inline.setups.map((s) => s.openTimeUnix),
    );
    expect(inWorker.processedMinutes).toBe(inline.processedMinutes);
  }, 30_000);
});
