import { Worker } from "node:worker_threads";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import type { SpikeParams } from "../detector/price-volume-spike.js";
import type { SpikeSetup } from "../detector/price-volume-spike.js";
import type { CandleBarDto } from "./source.js";

export interface WorkerReplayResult {
  setups: SpikeSetup[];
  processedMinutes: number;
  warmupMinutes: number;
  lastProcessedUnix: number | null;
  cancelled: boolean;
}

/**
 * Runs a replay on a bars array inside a worker thread so the API event loop
 * stays responsive. The bars payload is transferred once; streaming from the
 * market data API inside the worker is a follow-up (see PLAN).
 */
export function runReplayInWorker(input: {
  bars: CandleBarDto[];
  fromUnix: number;
  toUnix: number;
  params?: SpikeParams;
}): Promise<WorkerReplayResult> {
  return new Promise((resolve, reject) => {
    const workerPath = join(
      dirname(fileURLToPath(import.meta.url)),
      "worker.ts",
    );
    const worker = new Worker(workerPath, {
      workerData: input,
      execArgv: ["--import", "tsx"],
    });
    worker.once("message", (message: WorkerReplayResult) => {
      resolve(message);
    });
    worker.once("error", reject);
    worker.once("exit", (code) => {
      if (code !== 0) {
        reject(new Error(`replay worker exited with code ${code}`));
      }
    });
  });
}
