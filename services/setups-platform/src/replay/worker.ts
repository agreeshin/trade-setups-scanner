import { parentPort, workerData } from "node:worker_threads";
import { ArrayCandleSource, type CandleBarDto } from "./source.js";
import { replayRun } from "./replay-runner.js";
import type { SpikeParams } from "../detector/price-volume-spike.js";

export interface ReplayWorkerData {
  bars: CandleBarDto[];
  fromUnix: number;
  toUnix: number;
  params?: SpikeParams;
}

if (parentPort !== null) {
const data = workerData as ReplayWorkerData;
const source = new ArrayCandleSource(data.bars);
const result = await replayRun({
  source,
  fromUnix: data.fromUnix,
  toUnix: data.toUnix,
  params: data.params ?? undefined,
});
  parentPort.postMessage(result);
}
