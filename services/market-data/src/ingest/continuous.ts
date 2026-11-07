import type { Kysely } from "kysely";
import type { MarketDataDB } from "../db/schema.js";
import { getCoverage } from "../storage/coverage.js";
import { BackfillAdmissionError, requestBackfill, runBackfillJobOnce, type BackfillAdmissionLimits, type BackfillWorkerDeps } from "./backfill.js";
import type { Clock } from "./clock.js";

const MINUTE_MS = 60_000;
const CONTINUOUS_CLIENT = "continuous";

export async function setCollectionTarget(
  db: Kysely<MarketDataDB>,
  instrumentId: number,
  enabled: boolean,
): Promise<void> {
  await db
    .insertInto("collection_targets")
    .values({ instrument_id: instrumentId, enabled })
    .onConflict((oc) =>
      oc
        .column("instrument_id")
        .doUpdateSet({ enabled, updated_at: new Date() }),
    )
    .execute();
}

/** Upserts collection targets for many instruments in one statement. */
export async function setCollectionTargets(
  db: Kysely<MarketDataDB>,
  instrumentIds: number[],
  enabled: boolean,
): Promise<void> {
  await db
    .insertInto("collection_targets")
    .values(instrumentIds.map((instrument_id) => ({ instrument_id, enabled })))
    .onConflict((oc) =>
      oc
        .column("instrument_id")
        .doUpdateSet({ enabled, updated_at: new Date() }),
    )
    .execute();
}

/**
 * Creates catch-up backfill jobs for every enabled collection target that has
 * history but lags behind the last closed minute. Internal gaps require
 * explicit backfill requests and are not created here.
 */
export async function collectOnce(
  db: Kysely<MarketDataDB>,
  clock: Clock,
  limits?: BackfillAdmissionLimits,
): Promise<number> {
  const targets = await db
    .selectFrom("collection_targets")
    .where("enabled", "=", true)
    .selectAll()
    .execute();
  if (targets.length === 0) {
    return 0;
  }

  const lastClosedMinute = new Date(
    Math.floor(clock.nowMs() / MINUTE_MS) * MINUTE_MS - MINUTE_MS,
  );
  let scheduled = 0;

  for (const target of targets) {
    const coverage = await getCoverage(db, target.instrument_id);
    if (coverage.lastMinute === null) {
      continue;
    }
    const from = new Date(new Date(coverage.lastMinute).getTime() + MINUTE_MS);
    if (from.getTime() > lastClosedMinute.getTime()) {
      continue;
    }
    try {
      await requestBackfill(db, {
        instrumentId: target.instrument_id,
        fromTs: from,
        toTs: lastClosedMinute,
        requestedBy: CONTINUOUS_CLIENT,
      }, limits);
      scheduled += 1;
    } catch (error) {
      if (!(error instanceof BackfillAdmissionError)) throw error;
    }
  }
  return scheduled;
}

/** Drains all executable backfill jobs once; returns processed job count. */
export async function drainBackfills(deps: BackfillWorkerDeps): Promise<number> {
  let processed = 0;
  while ((await runBackfillJobOnce(deps)) === true) {
    processed += 1;
  }
  return processed;
}
