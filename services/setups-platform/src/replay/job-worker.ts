import { parentPort, workerData } from "node:worker_threads";
import { PriceVolumeSpikeDetector, type SpikeParams, type SpikeSetup } from "../detector/price-volume-spike.js";
import type { CandleBarDto } from "./source.js";

const input = workerData as { params: SpikeParams; fromUnix: number };
const detector = new PriceVolumeSpikeDetector(input.params);
parentPort!.on("message", (bars: CandleBarDto[]) => {
  const setups: SpikeSetup[] = [];
  let warmupMinutes = 0;
  let processedMinutes = 0;
  for (const bar of bars) {
    const result = detector.push(bar);
    if (result.status === "warmup") warmupMinutes++;
    else processedMinutes++;
    if (result.setup && bar.openTimeUnix >= input.fromUnix) setups.push(result.setup);
  }
  parentPort!.postMessage({ setups, warmupMinutes, processedMinutes });
});
