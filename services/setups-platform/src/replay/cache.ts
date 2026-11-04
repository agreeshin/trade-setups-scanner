import { sql, type Kysely } from "kysely";
import { randomUUID } from "node:crypto";
import type { SignalsDB } from "../db/schema.js";
import type { SpikeParams, SpikeSetup } from "../detector/price-volume-spike.js";

export interface CacheIdentity {
  exchangeCode: string;
  category: string;
  symbol: string;
}

// Clears and guarded publications serialize on the same transaction lock.
export async function lockReplayCache(db: Kysely<SignalsDB>): Promise<void> {
  await sql`select pg_advisory_xact_lock(72423411)`.execute(db);
}

export async function cacheGeneration(db: Kysely<SignalsDB>): Promise<string> {
  const row = await db.selectFrom("settings").select("value").where("key", "=", "replay-cache-generation").executeTakeFirst();
  return typeof row?.value.generation === "string" ? row.value.generation : "initial";
}

async function advanceGeneration(db: Kysely<SignalsDB>): Promise<void> {
  const value = { generation: randomUUID() };
  await db.insertInto("settings").values({ key: "replay-cache-generation", value })
    .onConflict((oc) => oc.column("key").doUpdateSet({ value, updated_at: new Date() })).execute();
}

export interface CacheQuery extends CacheIdentity {
  algorithmId: string;
  algorithmVersion: string;
  params: SpikeParams;
  fromUnix: number;
  toUnix: number;
}

export interface CachedReplayResult {
  cacheKey: string;
  setups: SpikeSetup[];
  warmupMinutes: number;
  processedMinutes: number;
  computedAt: string;
}

function canonicalParams(params: Record<string, unknown>): string {
  const keys = Object.keys(params).sort();
  const parts: string[] = [];
  for (const key of keys) {
    const value = params[key];
    if (value !== null && typeof value === "object" && !Array.isArray(value)) {
      parts.push(`${key}:{${canonicalParams(value as Record<string, unknown>)}}`);
    } else {
      parts.push(`${key}:${String(value)}`);
    }
  }
  return parts.join(",");
}

export function canonicalCacheKey(query: CacheQuery): string {
  return [
    `${query.algorithmId}@${query.algorithmVersion}`,
    `${query.exchangeCode}/${query.category}/${query.symbol}`,
    `${query.fromUnix}-${query.toUnix}`,
    canonicalParams(query.params as unknown as Record<string, unknown>),
  ].join("|");
}

export interface FinishedReplay {
  setups: SpikeSetup[];
  warmupMinutes: number;
  processedMinutes: number;
}

export async function cacheStore(
  db: Kysely<SignalsDB>,
  query: CacheQuery,
  result: FinishedReplay,
): Promise<void> {
  const cacheKey = canonicalCacheKey(query);
  // node-postgres serializes JS arrays as PostgreSQL array literals, not as
  // JSON, so jsonb columns receiving arrays must be stringified explicitly.
  const setupsJson = JSON.stringify(result.setups) as unknown as SpikeSetup[];
  await db
    .insertInto("replay_cache")
    .values({
      cache_key: cacheKey,
      exchange_code: query.exchangeCode,
      category: query.category,
      symbol: query.symbol,
      algorithm_id: query.algorithmId,
      algorithm_version: query.algorithmVersion,
      params: query.params,
      from_ts: new Date(query.fromUnix * 1000),
      to_ts: new Date(query.toUnix * 1000),
      setups: setupsJson,
      warmup_minutes: result.warmupMinutes,
      processed_minutes: result.processedMinutes,
    })
    .onConflict((oc) =>
      oc.column("cache_key").doUpdateSet({
        setups: setupsJson,
        warmup_minutes: result.warmupMinutes,
        processed_minutes: result.processedMinutes,
        computed_at: new Date(),
      }),
    )
    .execute();
}

export async function cacheLookup(
  db: Kysely<SignalsDB>,
  query: CacheQuery,
): Promise<CachedReplayResult | null> {
  const cacheKey = canonicalCacheKey(query);
  const row = await db
    .selectFrom("replay_cache")
    .where("cache_key", "=", cacheKey)
    .selectAll()
    .executeTakeFirst();
  if (!row) {
    return null;
  }
  return {
    cacheKey,
    setups: row.setups,
    warmupMinutes: row.warmup_minutes,
    processedMinutes: row.processed_minutes,
    computedAt: row.computed_at.toISOString(),
  };
}

export async function cacheBulkLookup(
  db: Kysely<SignalsDB>,
  queries: CacheQuery[],
): Promise<Map<string, CachedReplayResult>> {
  const hits = new Map<string, CachedReplayResult>();
  for (const query of queries) {
    const hit = await cacheLookup(db, query);
    if (hit !== null) {
      hits.set(hit.cacheKey, hit);
    }
  }
  return hits;
}

export async function cacheClearByInstrument(
  db: Kysely<SignalsDB>,
  identity: CacheIdentity,
): Promise<number> {
  return db.transaction().execute(async (tx) => {
  await lockReplayCache(tx);
  await advanceGeneration(tx);
  const result = await tx
    .deleteFrom("replay_cache")
    .where((eb) =>
      eb.and([
        eb("exchange_code", "=", identity.exchangeCode),
        eb("category", "=", identity.category),
        eb("symbol", "=", identity.symbol),
      ]),
    )
    .executeTakeFirst();
  return Number(result?.numDeletedRows ?? 0n);
  });
}

export async function cacheClearAll(db: Kysely<SignalsDB>): Promise<number> {
  return db.transaction().execute(async (tx) => {
    await lockReplayCache(tx);
    await advanceGeneration(tx);
    const result = await tx.deleteFrom("replay_cache").executeTakeFirst();
    return Number(result?.numDeletedRows ?? 0n);
  });
}

export interface CacheStats {
  entries: number;
  bytes: number;
}

export async function cacheStats(db: Kysely<SignalsDB>): Promise<CacheStats> {
  const countRow = await db
    .selectFrom("replay_cache")
    .select((eb) => eb.fn.countAll<string>().as("count"))
    .executeTakeFirst();
  const sizeRow = await sql<{ size: string }>`SELECT pg_total_relation_size('replay_cache') AS size`.execute(
    db,
  );
  return {
    entries: countRow ? Number(countRow.count) : 0,
    bytes: Number(sizeRow.rows[0]?.size ?? 0),
  };
}
