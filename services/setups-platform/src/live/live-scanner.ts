import type { Kysely } from "kysely";
import type { SignalsDB } from "../db/schema.js";
import type { SpikeParams, SpikeSetup } from "../detector/price-volume-spike.js";
import { DEFAULT_SPIKE_PARAMS } from "../detector/price-volume-spike.js";
import { replayRun } from "../replay/replay-runner.js";
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
  params?: SpikeParams;
  maxAgeSeconds: number;
  notifier?: Notifier;
}

export interface LiveScanResult {
  newSetups: SpikeSetup[];
  notified: number;
}

/**
 * Feeds the closed-minute window [now - history, now) through the detector for
 * each instrument and stores newly seen setups. Newly stored setups within the
 * notification age are handed to the notifier; retries never refresh the age
 * because it is computed from the immutable signal minute end.
 */
export async function liveScanOnce(options: LiveScanOptions): Promise<LiveScanResult> {
  const params = options.params ?? DEFAULT_SPIKE_PARAMS;
  const requiredMinutes = params.baselineLongDays * 1440;
  // Align the warmup start down to a whole minute so the first bar of the
  // window is never skipped and warmup length matches the history requirement.
  const warmupUnix =
    Math.floor((options.nowUnix - requiredMinutes * 60) / 60) * 60;
  const newSetups: SpikeSetup[] = [];
  let notified = 0;

  for (const instrument of options.instruments) {
    const run = await replayRun({
      source: options.source,
      fromUnix: warmupUnix,
      toUnix: options.nowUnix,
      params,
      warmupUnix,
    });
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
        const delivery = evaluateDeliveryAge(setup, options.nowUnix, options.maxAgeSeconds);
        if (delivery.eligible && options.notifier !== undefined) {
          await options.notifier.send(setup);
          notified += 1;
        }
      }
    }
  }

  return { newSetups, notified };
}
