import { type Kysely, sql } from "kysely";
import type { SignalsDB } from "../db/schema.js";
import type { SpikeParams, SpikeSetup } from "../detector/price-volume-spike.js";

export interface ReplayBatchSummary {
  id: number;
  exchange_code: string;
  category: string;
  algorithm_id: string;
  algorithm_version: string | null;
  params: SpikeParams;
  from_ts: Date;
  to_ts: Date;
  instrument_count: number;
  created_at: Date;
  status: string;
  cache_hit: boolean;
  setup_count: number;
  active_instruments: number;
  done_instruments: number;
  failed_instruments: number;
  cancelled_instruments: number;
}

export interface BatchSetupRow {
  setup: SpikeSetup;
  exchange_code: string;
  category: string;
  symbol: string;
  algorithm_id: string;
  algorithm_version: string | null;
  params: SpikeParams;
}

interface BatchAggregateRow extends Omit<ReplayBatchSummary, "status" | "cache_hit" | "setup_count" | "active_instruments" | "done_instruments" | "failed_instruments" | "cancelled_instruments"> {
  active_instruments: string | number;
  done_instruments: string | number;
  failed_instruments: string | number;
  cancelled_instruments: string | number;
  setup_count: string | number;
  cache_hit: boolean | null;
}

function toSummary(row: BatchAggregateRow): ReplayBatchSummary {
  const active = Number(row.active_instruments);
  const done = Number(row.done_instruments);
  const failed = Number(row.failed_instruments);
  return {
    ...row,
    active_instruments: active,
    done_instruments: done,
    failed_instruments: failed,
    cancelled_instruments: Number(row.cancelled_instruments),
    setup_count: Number(row.setup_count),
    cache_hit: row.cache_hit === true,
    status: active > 0 ? "running" : done > 0 ? "done" : failed > 0 ? "failed" : "cancelled",
  };
}

export async function listReplayBatches(db: Kysely<SignalsDB>, limit = 100): Promise<ReplayBatchSummary[]> {
  const result = await sql<BatchAggregateRow>`
    select b.id, b.exchange_code, b.category, b.algorithm_id, b.algorithm_version, b.params, b.from_ts, b.to_ts, b.instrument_count, b.created_at,
      count(*) filter (where r.status in ('queued','running','cancel_requested')) as active_instruments,
      count(*) filter (where r.status = 'done') as done_instruments,
      count(*) filter (where r.status = 'failed') as failed_instruments,
      count(*) filter (where r.status = 'cancelled') as cancelled_instruments,
      coalesce(sum(case when jsonb_typeof(r.setups) = 'array' then jsonb_array_length(r.setups) else 0 end), 0) as setup_count,
      bool_and(r.cache_hit) as cache_hit
    from replay_batches b join replay_runs r on r.batch_id = b.id
    group by b.id
    order by b.id desc
    limit ${limit}`.execute(db);
  return result.rows.map(toSummary);
}

export async function getReplayBatch(db: Kysely<SignalsDB>, batchId: number): Promise<ReplayBatchSummary | undefined> {
  const result = await sql<BatchAggregateRow>`
    select b.id, b.exchange_code, b.category, b.algorithm_id, b.algorithm_version, b.params, b.from_ts, b.to_ts, b.instrument_count, b.created_at,
      count(*) filter (where r.status in ('queued','running','cancel_requested')) as active_instruments,
      count(*) filter (where r.status = 'done') as done_instruments,
      count(*) filter (where r.status = 'failed') as failed_instruments,
      count(*) filter (where r.status = 'cancelled') as cancelled_instruments,
      coalesce(sum(case when jsonb_typeof(r.setups) = 'array' then jsonb_array_length(r.setups) else 0 end), 0) as setup_count,
      bool_and(r.cache_hit) as cache_hit
    from replay_batches b join replay_runs r on r.batch_id = b.id
    where b.id = ${batchId}
    group by b.id`.execute(db);
  return result.rows[0] === undefined ? undefined : toSummary(result.rows[0]);
}

export async function replayBatchResultPage(
  db: Kysely<SignalsDB>, batchId: number, offset: number, limit: number,
): Promise<{ setups: BatchSetupRow[]; total: number } | undefined> {
  const batch = await db.selectFrom("replay_batches").select("id").where("id", "=", batchId).executeTakeFirst();
  if (batch === undefined) return undefined;
  const total = await sql<{ total: string | number }>`
    select coalesce(sum(case when jsonb_typeof(setups) = 'array' then jsonb_array_length(setups) else 0 end), 0) as total
    from replay_runs where batch_id = ${batchId}`.execute(db);
  const page = await sql<BatchSetupRow>`
    select s.value as setup, r.exchange_code, r.category, r.symbol, r.algorithm_id, r.algorithm_version, r.params
    from replay_runs r
    cross join lateral jsonb_array_elements(case when jsonb_typeof(r.setups) = 'array' then r.setups else '[]'::jsonb end) with ordinality as s(value, ordinality)
    where r.batch_id = ${batchId}
    order by (s.value->>'closeTimeUnix')::bigint desc, r.id asc, s.ordinality asc
    limit ${limit} offset ${offset}`.execute(db);
  return { setups: page.rows, total: Number(total.rows[0]?.total ?? 0) };
}

export async function cancelReplayBatch(
  db: Kysely<SignalsDB>, batchId: number,
): Promise<{ cancelled: number; cancelRequested: number } | undefined> {
  return db.transaction().execute(async (tx) => {
    await sql`select pg_advisory_xact_lock(72423412)`.execute(tx);
    const batch = await tx.selectFrom("replay_batches").select("id").where("id", "=", batchId).executeTakeFirst();
    if (batch === undefined) return undefined;
    const queued = await tx.updateTable("replay_runs").set({ status: "cancelled", updated_at: new Date() })
      .where("batch_id", "=", batchId).where("status", "=", "queued").returning("id").execute();
    const running = await tx.updateTable("replay_runs").set({ status: "cancel_requested", updated_at: new Date() })
      .where("batch_id", "=", batchId).where("status", "=", "running").returning("id").execute();
    return { cancelled: queued.length, cancelRequested: running.length };
  });
}

export type ReplayBatchDeleteOutcome =
  | { status: "deleted"; runsDeleted: number; qualityIntervalsDeleted: number }
  | { status: "active" };

/** Fully removes one finished batch with its child runs and their quality intervals; shared computation cache is untouched. */
export async function deleteReplayBatch(
  db: Kysely<SignalsDB>, batchId: number,
): Promise<ReplayBatchDeleteOutcome | undefined> {
  return db.transaction().execute(async (tx) => {
    await sql`select pg_advisory_xact_lock(72423412)`.execute(tx);
    const batch = await tx.selectFrom("replay_batches").select("id").where("id", "=", batchId).executeTakeFirst();
    if (batch === undefined) return undefined;
    const children = await tx.selectFrom("replay_runs").select(["id", "status"]).where("batch_id", "=", batchId).execute();
    if (children.some((row) => row.status === "queued" || row.status === "running" || row.status === "cancel_requested")) {
      return { status: "active" };
    }
    const childIds = children.map((row) => row.id);
    const quality = childIds.length > 0
      ? await tx.deleteFrom("replay_quality_intervals").where("run_id", "in", childIds).returning("run_id").execute()
      : [];
    const runs = await tx.deleteFrom("replay_runs").where("batch_id", "=", batchId).returning("id").execute();
    await tx.deleteFrom("replay_batches").where("id", "=", batchId).execute();
    return { status: "deleted", runsDeleted: runs.length, qualityIntervalsDeleted: quality.length };
  });
}
