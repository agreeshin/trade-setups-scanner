import type { Kysely } from "kysely";
import type { MarketDataDB, BackfillJobStatus } from "../db/schema.js";
import { insertCandles } from "../storage/candles.js";
import type { BybitClient } from "./bybit.js";

const MINUTE_MS = 60_000;
const ACTIVE_STATUSES: BackfillJobStatus[] = ["queued", "running", "cancel_requested"];
const DEFAULT_CHUNK_MINUTES = 1440;
const MAX_CHUNK_FAILURES = 3;

export interface BackfillRequest {
  instrumentId: number;
  fromTs: Date;
  toTs: Date;
  requestedBy: string;
}

export async function requestBackfill(
  db: Kysely<MarketDataDB>,
  request: BackfillRequest,
): Promise<number> {
  const existing = await db
    .selectFrom("backfill_jobs")
    .where("instrument_id", "=", request.instrumentId)
    .where("status", "in", ACTIVE_STATUSES)
    .where("from_ts", "<=", request.fromTs)
    .where("to_ts", ">=", request.toTs)
    .orderBy("id", "asc")
    .limit(1)
    .selectAll()
    .executeTakeFirst();

  if (existing) {
    const requestedBy = existing.requested_by.includes(request.requestedBy)
      ? existing.requested_by
      : [...existing.requested_by, request.requestedBy];
    await db
      .updateTable("backfill_jobs")
      .set({
        requested_by: requestedBy,
        status: existing.status === "cancel_requested" ? "queued" : existing.status,
        updated_at: new Date(),
      })
      .where("id", "=", existing.id)
      .execute();
    return existing.id;
  }

  const created = await db
    .insertInto("backfill_jobs")
    .values({
      instrument_id: request.instrumentId,
      from_ts: request.fromTs,
      to_ts: request.toTs,
      status: "queued",
      requested_by: [request.requestedBy],
    })
    .returning("id")
    .executeTakeFirstOrThrow();
  return created.id;
}

export async function cancelBackfillRequest(
  db: Kysely<MarketDataDB>,
  jobId: number,
  requestedBy: string,
): Promise<void> {
  const job = await db
    .selectFrom("backfill_jobs")
    .where("id", "=", jobId)
    .selectAll()
    .executeTakeFirstOrThrow();
  if (!ACTIVE_STATUSES.includes(job.status)) {
    return;
  }
  const remaining = job.requested_by.filter((client) => client !== requestedBy);
  if (remaining.length === 0) {
    await db
      .updateTable("backfill_jobs")
      .set({
        status: "cancel_requested",
        requested_by: remaining,
        updated_at: new Date(),
      })
      .where("id", "=", jobId)
      .execute();
  } else {
    await db
      .updateTable("backfill_jobs")
      .set({ requested_by: remaining, updated_at: new Date() })
      .where("id", "=", jobId)
      .execute();
  }
}

export interface BackfillWorkerDeps {
  db: Kysely<MarketDataDB>;
  client: BybitClient;
  chunkMinutes?: number;
}

/**
 * Processes at most one queued backfill job. Returns true when a job was
 * picked up (regardless of final status), false when the queue is empty.
 */
export async function runBackfillJobOnce(deps: BackfillWorkerDeps): Promise<boolean> {
  const { db, client } = deps;
  const chunkMinutes = deps.chunkMinutes ?? DEFAULT_CHUNK_MINUTES;

  const job = await db
    .selectFrom("backfill_jobs")
    .where("status", "in", ["queued", "cancel_requested"])
    .orderBy("id", "asc")
    .limit(1)
    .selectAll()
    .executeTakeFirst();
  if (!job) {
    return false;
  }

  if (job.status === "cancel_requested" && job.requested_by.length === 0) {
    await db
      .updateTable("backfill_jobs")
      .set({ status: "cancelled", updated_at: new Date() })
      .where("id", "=", job.id)
      .execute();
    return true;
  }

  await db
    .updateTable("backfill_jobs")
    .set({ status: "running", updated_at: new Date() })
    .where("id", "=", job.id)
    .execute();

  const instrument = await db
    .selectFrom("instruments")
    .innerJoin("exchanges", "exchanges.id", "instruments.exchange_id")
    .where("instruments.id", "=", job.instrument_id)
    .select(["exchanges.code as exchange", "instruments.symbol as symbol"])
    .executeTakeFirstOrThrow();

  let cursorMs =
    job.checkpoint_ts !== null
      ? job.checkpoint_ts.getTime() + MINUTE_MS
      : job.from_ts.getTime();
  const endMs = job.to_ts.getTime();
  let chunkFailures = 0;

  while (cursorMs <= endMs) {
    const fresh = await db
      .selectFrom("backfill_jobs")
      .where("id", "=", job.id)
      .select(["status", "requested_by"])
      .executeTakeFirstOrThrow();
    if (fresh.status === "cancel_requested" && fresh.requested_by.length === 0) {
      await db
        .updateTable("backfill_jobs")
        .set({ status: "cancelled", updated_at: new Date() })
        .where("id", "=", job.id)
        .execute();
      return true;
    }

    const chunkEndMs = Math.min(endMs, cursorMs + (chunkMinutes - 1) * MINUTE_MS);
    try {
      const bars = await client.fetchClosedBars(
        instrument.symbol,
        cursorMs,
        chunkEndMs,
      );
      const result = await insertCandles(
        db,
        job.instrument_id,
        bars.map((bar) => ({
          openTime: bar.openTime,
          open: bar.open,
          high: bar.high,
          low: bar.low,
          close: bar.close,
          volume: bar.volume,
          quoteTurnover: bar.quoteTurnover,
        })),
      );
      void result;
      cursorMs = chunkEndMs + MINUTE_MS;
      chunkFailures = 0;
      await db
        .updateTable("backfill_jobs")
        .set({ checkpoint_ts: new Date(chunkEndMs), updated_at: new Date() })
        .where("id", "=", job.id)
        .execute();
    } catch (error) {
      chunkFailures += 1;
      if (chunkFailures > MAX_CHUNK_FAILURES) {
        await db
          .updateTable("backfill_jobs")
          .set({
            status: "failed",
            error: error instanceof Error ? error.message : String(error),
            updated_at: new Date(),
          })
          .where("id", "=", job.id)
          .execute();
        return true;
      }
    }
  }

  await db
    .updateTable("backfill_jobs")
    .set({ status: "done", checkpoint_ts: job.to_ts, updated_at: new Date() })
    .where("id", "=", job.id)
    .execute();
  return true;
}
