import {
  PriceVolumeSpikeDetector,
  DEFAULT_SPIKE_PARAMS,
  type SpikeParams,
  type SpikeSetup,
} from "../detector/price-volume-spike.js";
import type { CandleSource } from "./source.js";

const MINUTE_SECONDS = 60;
const PAGE_LIMIT = 5000;

export interface CancelToken {
  cancelled: boolean;
}

export interface ReplayRunOptions {
  source: CandleSource;
  /** Signal window [fromUnix, toUnix). */
  fromUnix: number;
  toUnix: number;
  params?: SpikeParams | undefined;
  /** Warmup start; defaults to fromUnix minus the required history. */
  warmupUnix?: number | undefined;
  cancel?: CancelToken | undefined;
  onCheckpoint?: ((lastProcessedUnix: number) => void) | undefined;
}

export interface ReplayRunResult {
  setups: SpikeSetup[];
  processedMinutes: number;
  warmupMinutes: number;
  lastProcessedUnix: number | null;
  cancelled: boolean;
}

/**
 * Streams warmup plus signal window bars through the detector page by page.
 * Memory is bounded: only the detector's fixed history window and the current
 * page are retained. Resume semantics: re-run with a warmup starting
 * `checkpoint - history`; the detector rebuilds deterministically, so results
 * are identical and idempotent.
 */
export async function replayRun(options: ReplayRunOptions): Promise<ReplayRunResult> {
  const params = options.params ?? DEFAULT_SPIKE_PARAMS;
  const requiredMinutes = params.baselineLongDays * 1440;
  const detector = new PriceVolumeSpikeDetector(params);
  const warmupUnix =
    options.warmupUnix ?? options.fromUnix - requiredMinutes * MINUTE_SECONDS;

  const setups: SpikeSetup[] = [];
  let processedMinutes = 0;
  let warmupMinutes = 0;
  let lastProcessedUnix: number | null = null;
  let cursor: number | undefined;

  outer: for (;;) {
    if (options.cancel?.cancelled) {
      return {
        setups,
        processedMinutes,
        warmupMinutes,
        lastProcessedUnix,
        cancelled: true,
      };
    }
    const page = await options.source.read({
      fromUnix: warmupUnix,
      toUnix: options.toUnix,
      afterUnix: cursor,
      limit: PAGE_LIMIT,
    });
    if (page.bars.length === 0) {
      break;
    }
    for (const bar of page.bars) {
      if (options.cancel?.cancelled) {
        break outer;
      }
      const result = detector.push({
        openTimeUnix: bar.openTimeUnix,
        close: bar.close,
        quoteTurnover: bar.quoteTurnover,
      });
      if (result.status === "warmup") {
        warmupMinutes += 1;
      } else {
        processedMinutes += 1;
      }
      if (result.setup !== undefined && bar.openTimeUnix >= options.fromUnix) {
        setups.push(result.setup);
      }
      lastProcessedUnix = bar.openTimeUnix;
      options.onCheckpoint?.(lastProcessedUnix);
    }
    if (page.nextCursorUnix === null) {
      break;
    }
    cursor = page.nextCursorUnix;
  }

  return {
    setups,
    processedMinutes,
    warmupMinutes,
    lastProcessedUnix,
    cancelled: options.cancel?.cancelled === true,
  };
}
