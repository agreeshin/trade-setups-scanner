import { sql, type Kysely } from "kysely";
import type { MarketDataDB, BackfillJobStatus } from "../db/schema.js";
import { insertCandles } from "../storage/candles.js";
import { getCoverage, getCoverageRange, type Coverage } from "../storage/coverage.js";
import type { BybitClient } from "./bybit.js";

const MINUTE_MS = 60_000;
export const ACTIVE_STATUSES: BackfillJobStatus[] = ["queued", "running", "cancel_requested"];
const DEFAULT_CHUNK_MINUTES = 1440;
const MAX_CHUNK_FAILURES = 3;

export interface BackfillRequest {
  instrumentId: number;
  fromTs: Date;
  toTs: Date;
  requestedBy: string;
}

export interface BackfillAdmissionLimits {
  maxPendingJobs: number;
  maxWorkMinutes: number;
  maxDatabaseBytes: number;
  estimatedBytesPerWorkMinute: number;
}

export const DEFAULT_BACKFILL_ADMISSION_LIMITS: BackfillAdmissionLimits = {
  maxPendingJobs: 100,
  maxWorkMinutes: 2_635_200,
  maxDatabaseBytes: 100 * 1024 ** 3,
  estimatedBytesPerWorkMinute: 512,
};

export class BackfillAdmissionError extends Error {
  constructor(
    readonly resource: "queue_jobs" | "work_minutes" | "database_bytes",
    readonly limit: number,
    readonly requested: number,
  ) {
    super(`backfill ${resource} limit exceeded: requested ${requested}, limit ${limit}`);
  }
}

export async function requestBackfill(
  db: Kysely<MarketDataDB>,
  request: BackfillRequest,
  limits: BackfillAdmissionLimits = DEFAULT_BACKFILL_ADMISSION_LIMITS,
): Promise<number> {
  return db.transaction().execute(async (tx) => {
    await sql`select pg_advisory_xact_lock(72423413)`.execute(tx);
    const existing = await tx
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
      await tx.updateTable("backfill_jobs").set({
        requested_by: requestedBy,
        status: existing.status === "cancel_requested" ? "queued" : existing.status,
        updated_at: new Date(),
      }).where("id", "=", existing.id).execute();
      return existing.id;
    }

    const count = await tx.selectFrom("backfill_jobs").select((eb) => eb.fn.countAll<string>().as("n"))
      .where("status", "in", ACTIVE_STATUSES).executeTakeFirstOrThrow();
    const requestedJobs = Number(count.n) + 1;
    if (requestedJobs > limits.maxPendingJobs) {
      throw new BackfillAdmissionError("queue_jobs", limits.maxPendingJobs, requestedJobs);
    }

    const coverage = await getCoverageRange(tx, request.instrumentId, {
      fromUnix: request.fromTs.getTime() / 1000,
      toUnix: request.toTs.getTime() / 1000 + 60,
      splitUnix: request.fromTs.getTime() / 1000,
      limit: 1,
    });
    if (!coverage) throw new Error(`instrument ${request.instrumentId} not found`);
    const workMinutes = coverage.missingMinutes;
    if (!Number.isSafeInteger(workMinutes) || workMinutes > limits.maxWorkMinutes) {
      throw new BackfillAdmissionError("work_minutes", limits.maxWorkMinutes, workMinutes);
    }

    const size = await sql<{ bytes: string }>`select pg_database_size(current_database())::text as bytes`.execute(tx);
    const databaseBytes = Number(size.rows[0]?.bytes ?? 0);
    const active = await tx.selectFrom("backfill_jobs")
      .select(["from_ts", "to_ts"])
      .where("status", "in", ACTIVE_STATUSES)
      .execute();
    const reservedBytes = active.reduce((sum, job) => sum
      + ((job.to_ts.getTime() - job.from_ts.getTime()) / MINUTE_MS + 1)
      * limits.estimatedBytesPerWorkMinute + 4096, 0);
    const requestedBytes = databaseBytes + reservedBytes
      + workMinutes * limits.estimatedBytesPerWorkMinute + 4096;
    if (!Number.isSafeInteger(requestedBytes) || requestedBytes > limits.maxDatabaseBytes) {
      throw new BackfillAdmissionError("database_bytes", limits.maxDatabaseBytes, requestedBytes);
    }

    const created = await tx.insertInto("backfill_jobs").values({
      instrument_id: request.instrumentId,
      from_ts: request.fromTs,
      to_ts: request.toTs,
      status: "queued",
      requested_by: [request.requestedBy],
    }).returning("id").executeTakeFirstOrThrow();
    return created.id;
  });
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

export interface MissingInterval {
  startMs: number;
  endMs: number;
}

/**
 * Computes minute ranges inside [fromMs, toMs] that are not covered by the
 * instrument's stored history, so already stored minutes are never refetched.
 */
export function missingIntervals(
  coverage: Coverage,
  fromMs: number,
  toMs: number,
): MissingInterval[] {
  const covered = coverage.intervals
    .map((interval) => ({
      start: new Date(interval.start).getTime(),
      end: new Date(interval.end).getTime(),
    }))
    .sort((a, b) => a.start - b.start);

  const missing: MissingInterval[] = [];
  let cursor = fromMs;

  for (const interval of covered) {
    if (interval.end < cursor || interval.start > toMs) {
      continue;
    }
    if (interval.start > cursor) {
      missing.push({
        startMs: cursor,
        endMs: Math.min(interval.start - MINUTE_MS, toMs),
      });
    }
    cursor = Math.max(cursor, interval.end + MINUTE_MS);
    if (cursor > toMs) {
      return missing;
    }
  }

  if (cursor <= toMs) {
    missing.push({ startMs: cursor, endMs: toMs });
  }
  return missing;
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
      const coverage = await getCoverage(db, job.instrument_id);
      const missing = missingIntervals(coverage, cursorMs, chunkEndMs);
      for (const gap of missing) {
        const bars = await client.fetchClosedBars(
          instrument.symbol,
          gap.startMs,
          gap.endMs,
        );
        await insertCandles(
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
      }
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
