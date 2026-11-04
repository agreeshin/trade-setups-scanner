import { sql, type Kysely } from "kysely";
import type { SignalsDB } from "../db/schema.js";
import type { SpikeParams, SpikeSetup } from "../detector/price-volume-spike.js";
import { SPIKE_ALGORITHM_ID, SPIKE_ALGORITHM_VERSION } from "../detector/price-volume-spike.js";
import { getAlgorithm } from "../algorithms.js";
import { replayRun, type CancelToken } from "../replay/replay-runner.js";
import type { CandleSource } from "../replay/source.js";
import { evaluateDeliveryAge, type Notifier } from "../notifications.js";

export interface LiveInstrument {
  exchangeCode: string;
  category: string;
  symbol: string;
}

export interface LiveScanOptions {
  db: Kysely<SignalsDB>;
  source: CandleSource;
  instruments: LiveInstrument[];
  /** Last closed minute as unix seconds. */
  nowUnix: number;
  /** Wall-clock delivery time, independent of collector lag. */
  deliveryNowUnix?: number;
  fromUnix?: number;
  params?: SpikeParams;
  maxAgeSeconds: number;
  notifier?: Notifier;
  cancel?: CancelToken;
}

export interface LiveScanResult {
  newSetups: SpikeSetup[];
  notified: number;
  lastProcessedUnix: number | null;
  ready: boolean;
}

/**
 * Feeds the closed-minute window [now - history, now) through the detector for
 * each instrument and stores newly seen setups. Newly stored setups within the
 * notification age are handed to the notifier; retries never refresh the age
 * because it is computed from the immutable signal minute end.
 */
export async function liveScanOnce(options: LiveScanOptions): Promise<LiveScanResult> {
  const params = options.params ?? (await getAlgorithm(options.db))!.params;
  const requiredMinutes = params.baselineLongDays * 1440;
  // Align the warmup start down to a whole minute so the first bar of the
  // window is never skipped and warmup length matches the history requirement.
  const fromUnix = options.fromUnix ?? options.nowUnix - 600;
  const warmupUnix = Math.floor((fromUnix - requiredMinutes * 60) / 60) * 60;
  const newSetups: SpikeSetup[] = [];
  let notified = 0;
  let lastProcessedUnix: number | null = null;
  let ready = false;

  for (const instrument of options.instruments) {
    const previous = await options.db.selectFrom("live_setups")
      .where("exchange_code", "=", instrument.exchangeCode).where("category", "=", instrument.category)
      .where("symbol", "=", instrument.symbol).where("algorithm_id", "=", SPIKE_ALGORITHM_ID)
      .where("algorithm_version", "=", SPIKE_ALGORITHM_VERSION)
      .where("params", "=", sql<SpikeParams>`${JSON.stringify(params)}::jsonb`)
      .orderBy("open_time", "desc").select("open_time").executeTakeFirst();
    const run = await replayRun({
      source: options.source,
      fromUnix,
      toUnix: options.nowUnix,
      params,
      warmupUnix,
      cancel: options.cancel,
      lastSetupUnix: previous ? previous.open_time.getTime() / 1000 : undefined,
    });
    if (run.cancelled) break;
    lastProcessedUnix = run.lastProcessedUnix;
    ready = run.lastStatus === "evaluated";
    for (const setup of run.setups) {
      const inserted = await options.db
        .insertInto("live_setups")
        .values({
          exchange_code: instrument.exchangeCode,
          category: instrument.category,
          symbol: instrument.symbol,
          algorithm_id: setup.algorithmId,
          algorithm_version: setup.algorithmVersion,
          params: setup.params,
          open_time: new Date(setup.openTimeUnix * 1000),
          close_time: new Date(setup.closeTimeUnix * 1000),
          close_price: setup.closePrice,
          details: setup.details ?? null,
        })
        .onConflict((oc) =>
          oc
            .columns([
              "exchange_code",
              "category",
              "symbol",
              "algorithm_id",
              "algorithm_version",
              "open_time",
            ])
            .doNothing(),
        )
        .returning("id")
        .executeTakeFirst();
      if (inserted !== undefined) {
        newSetups.push(setup);
        const delivery = evaluateDeliveryAge(setup, options.deliveryNowUnix ?? options.nowUnix, options.maxAgeSeconds);
        if (delivery.eligible && options.notifier !== undefined) {
          await options.notifier.send(setup);
          notified += 1;
        }
      }
    }
  }

  return { newSetups, notified, lastProcessedUnix, ready };
}
