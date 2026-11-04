import { parentPort, workerData } from "node:worker_threads";
import { PriceVolumeSpikeDetector, type SpikeParams, type SpikeSetup } from "../detector/price-volume-spike.js";
import type { CandleBarDto } from "./source.js";
import { ReplayQualityTracker } from "./quality.js";

const input = workerData as { params: SpikeParams; fromUnix: number; toUnix: number };
const detector = new PriceVolumeSpikeDetector(input.params);
const tracker = new ReplayQualityTracker(input.fromUnix, input.toUnix, input.params.baselineLongDays * 1440);
parentPort!.on("message", ({ bars, finished }: { bars: CandleBarDto[]; finished?: boolean }) => {
  const setups: SpikeSetup[] = [];
  let warmupMinutes = 0;
  let processedMinutes = 0;
  for (const bar of bars) {
    const result = detector.push(bar);
    tracker.observe(bar.openTimeUnix, result);
    if (result.status === "warmup") warmupMinutes++;
    else processedMinutes++;
    if (result.setup && bar.openTimeUnix >= input.fromUnix) setups.push(result.setup);
  }
  if (finished) tracker.finish();
  parentPort!.postMessage({ setups, warmupMinutes, processedMinutes, quality: tracker.quality, intervals: tracker.drain() });
});
