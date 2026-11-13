import { sql, type Kysely } from "kysely";
import type { MarketDataDB } from "../db/schema.js";
import { initializeCollectionTails } from "../storage/watermarks.js";
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
  await initializeCollectionTails(db, targets.filter((target) => !target.tail_initialized).map((target) => target.instrument_id));
  const lastClosedMinute = new Date(
    Math.floor(clock.nowMs() / MINUTE_MS) * MINUTE_MS - MINUTE_MS,
  );
  // Healthy WS collection needs no per-minute backfill job or full-history
  // coverage scan. Advance only a short, explicitly counted real-candle prefix.
  await sql`UPDATE collection_targets t SET tail_next_ts = ${lastClosedMinute}::timestamptz + interval '1 minute'
    WHERE t.enabled AND t.tail_next_ts <= ${lastClosedMinute}
      AND t.tail_next_ts >= ${lastClosedMinute}::timestamptz - interval '30 minutes'
      AND (SELECT count(*) FROM candles c WHERE c.instrument_id = t.instrument_id
        AND c.open_time >= t.tail_next_ts AND c.open_time <= ${lastClosedMinute})
        = extract(epoch FROM (${lastClosedMinute}::timestamptz - t.tail_next_ts)) / 60 + 1`.execute(db);
  const initialized = await db.selectFrom("collection_targets").where("enabled", "=", true).selectAll().execute();
  let scheduled = 0;

  for (const target of initialized) {
    if (target.tail_next_ts === null) {
      continue;
    }
    // This durable repair cursor is deliberately NOT max(open_time).
    const from = target.tail_next_ts;
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
