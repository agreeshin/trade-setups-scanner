import { sql, type Kysely } from "kysely";
import type { MarketDataDB, CandleRecordJson } from "../db/schema.js";
import { ensureMonthPartitions } from "./partitions.js";
import { ensureWatermarks } from "./watermarks.js";
import { recordCanonicalInserts } from "./read-guards.js";

export interface CandleInsert {
  openTime: Date;
  open: string;
  high: string;
  low: string;
  close: string;
  volume: string;
  quoteTurnover: string;
}

export interface InsertResult {
  inserted: number;
  conflicting: number;
}

export interface CandleRow extends CandleInsert {
  instrumentId: number;
  openTimeUnix: number;
  source: string;
}

interface StoredShape {
  open: string;
  high: string;
  low: string;
  close: string;
  volume: string;
  quote_turnover: string;
}

function normalizeDecimal(value: string): string {
  const trimmed = value.trim().replace(/^0+(?=\d)/, "");
  if (!trimmed.includes(".")) {
    return trimmed;
  }
  const stripped = trimmed.replace(/0+$/, "");
  return stripped.endsWith(".") ? stripped.slice(0, -1) : stripped;
}

function candleValues(instrumentId: number, candle: CandleInsert) {
  return {
    instrument_id: instrumentId,
    open_time: candle.openTime,
    open: candle.open,
    high: candle.high,
    low: candle.low,
    close: candle.close,
    volume: candle.volume,
    quote_turnover: candle.quoteTurnover,
    source: "bybit",
  };
}

function differsFromStored(stored: StoredShape, incoming: CandleInsert): boolean {
  return (
    normalizeDecimal(stored.open) !== normalizeDecimal(incoming.open) ||
    normalizeDecimal(stored.high) !== normalizeDecimal(incoming.high) ||
    normalizeDecimal(stored.low) !== normalizeDecimal(incoming.low) ||
    normalizeDecimal(stored.close) !== normalizeDecimal(incoming.close) ||
    normalizeDecimal(stored.volume) !== normalizeDecimal(incoming.volume) ||
    normalizeDecimal(stored.quote_turnover) !== normalizeDecimal(incoming.quoteTurnover)
  );
}

function toConflictJson(values: StoredShape): CandleRecordJson {
  return {
    open: values.open,
    high: values.high,
    low: values.low,
    close: values.close,
    volume: values.volume,
    quote_turnover: values.quote_turnover,
  };
}

export async function insertCandles(
  db: Kysely<MarketDataDB>,
  instrumentId: number,
  candles: CandleInsert[],
): Promise<InsertResult> {
  return insertCandleBatch(db, candles.map((candle) => ({ instrumentId, candle })));
}

export interface CandleObservation { instrumentId: number; candle: CandleInsert }

/** Exact normalized observation identity; differing duplicates must survive. */
export function candleObservationKey(instrumentId: number, candle: CandleInsert): string {
  return JSON.stringify([instrumentId, candle.openTime.getTime(), ...[candle.open, candle.high, candle.low,
    candle.close, candle.volume, candle.quoteTurnover].map(normalizeDecimal)]);
}

/** Shared WS/REST add-only writer. Input order chooses the first offered value,
 * while database lock order is always instrument/time ascending. */
export async function insertCandleBatch(
  db: Kysely<MarketDataDB>, observations: CandleObservation[], statementTimeoutMs?: number,
): Promise<InsertResult> {
  if (!observations.length) return { inserted: 0, conflicting: 0 };
  const distinct = new Map<string, CandleObservation>();
  const canonical = new Map<string, CandleObservation>();
  const key = (id: number, time: Date) => `${id}:${time.getTime()}`;
  for (const observation of observations) {
    distinct.set(candleObservationKey(observation.instrumentId, observation.candle), observation);
    const k = key(observation.instrumentId, observation.candle.openTime);
    if (!canonical.has(k)) canonical.set(k, observation);
  }
  const ordered = [...canonical.values()].sort((a, b) => a.instrumentId - b.instrumentId || a.candle.openTime.getTime() - b.candle.openTime.getTime());
  const ids = [...new Set(ordered.map((row) => row.instrumentId))];
  const times = ordered.map((row) => row.candle.openTime.getTime());
  const write = async (connection: Kysely<MarketDataDB>): Promise<InsertResult> => {
    const deadline = statementTimeoutMs === undefined ? Infinity : Date.now() + statementTimeoutMs;
    const checkDeadline = () => { if (Date.now() >= deadline) throw new Error("candle batch transaction deadline exceeded"); };
    // Server-side cancellation plus cooperative checks BETWEEN queries. Never
    // race an uncancelled transaction against a JS timeout and report it done.
    const tx = connection.withPlugin({
      transformQuery: (args) => { checkDeadline(); return args.node; },
      transformResult: (args) => { checkDeadline(); return Promise.resolve(args.result); },
    });
    if (statementTimeoutMs !== undefined)
      await sql`SELECT set_config('statement_timeout', ${String(statementTimeoutMs)}, true),
        set_config('lock_timeout', ${String(statementTimeoutMs)}, true),
        set_config('idle_in_transaction_session_timeout', ${String(statementTimeoutMs)}, true)`.execute(tx);
    await ensureMonthPartitions(tx, new Date(Math.min(...times)), new Date(Math.max(...times)));
    await ensureWatermarks(tx, ids);
    const locked = await tx.selectFrom("instrument_watermarks").where("instrument_id", "in", ids)
      .orderBy("instrument_id").select("instrument_id").forUpdate().execute();
    if (locked.length !== ids.length) throw new Error("candle batch watermark missing");
    const heads = await tx.selectFrom("candle_insert_heads").where("instrument_id", "in", ids).select("instrument_id").execute();
    if (heads.length !== ids.length) throw new Error("canonical insert metadata missing for batch");
    const inserted = await tx.insertInto("candles").values(ordered.map((row) => candleValues(row.instrumentId, row.candle)))
      .onConflict((oc) => oc.constraint("candles_pkey").doNothing()).returning(["instrument_id", "open_time"]).execute();
    await recordCanonicalInserts(tx, inserted);
    // Read back AFTER the serialized insert, including concurrent REST values.
    const keys = ordered.map((row) => ({ id: row.instrumentId, at: row.candle.openTime.toISOString() }));
    const existing = await sql<StoredShape & { instrument_id: number; open_time: Date }>`SELECT c.* FROM
      jsonb_to_recordset(${JSON.stringify(keys)}::jsonb) AS k(id integer, at timestamptz)
      JOIN candles c ON c.instrument_id = k.id AND c.open_time = k.at`.execute(tx);
    const byKey = new Map(existing.rows.map((row) => [key(row.instrument_id, row.open_time), row]));
    const conflicts = [...distinct.values()].filter((row) => {
      const stored = byKey.get(key(row.instrumentId, row.candle.openTime));
      if (!stored) throw new Error("canonical candle missing after insertion");
      return differsFromStored(stored, row.candle);
    });
    if (conflicts.length) await tx.insertInto("candle_conflicts").values(conflicts.map(({ instrumentId, candle }) => ({
      instrument_id: instrumentId, open_time: candle.openTime,
      existing_record: toConflictJson(byKey.get(key(instrumentId, candle.openTime))!),
      incoming_record: toConflictJson({ ...candle, quote_turnover: candle.quoteTurnover }),
    }))).onConflict((oc) => oc.constraint("candle_conflicts_unique").doNothing()).execute();
    const latest = new Map<number, Date>();
    for (const row of ordered) latest.set(row.instrumentId, row.candle.openTime);
    const values = [...latest].map(([id, at]) => ({ id, at: at.toISOString() }));
    await sql`UPDATE instrument_watermarks w SET latest_open_time = k.at, latest_received_at = statement_timestamp()
      FROM jsonb_to_recordset(${JSON.stringify(values)}::jsonb) AS k(id integer, at timestamptz)
      WHERE w.instrument_id = k.id AND (w.latest_open_time IS NULL OR w.latest_open_time < k.at)`.execute(tx);
    await sql`UPDATE collection_targets t SET last_collected_ts = k.at
      FROM jsonb_to_recordset(${JSON.stringify(values)}::jsonb) AS k(id integer, at timestamptz)
      WHERE t.instrument_id = k.id AND (t.last_collected_ts IS NULL OR t.last_collected_ts < k.at)`.execute(tx);
    return { inserted: inserted.length, conflicting: conflicts.length };
  };
  return db.isTransaction ? write(db) : db.transaction().execute(write);
}

export interface ReadCandlesQuery {
  instrumentId: number;
  from: Date;
  to: Date;
  limit?: number | undefined;
  afterUnix?: number | undefined;
}

export interface ReadCandlesPage {
  candles: CandleRow[];
  nextCursorUnix: number | null;
}

export async function readCandles(
  db: Kysely<MarketDataDB>,
  query: ReadCandlesQuery,
): Promise<ReadCandlesPage> {
  const limit = Math.min(query.limit ?? 1000, 5000);
  const after = query.afterUnix !== undefined ? new Date(query.afterUnix * 1000) : null;

  const rows = await db
    .selectFrom("candles")
    .where((eb) => {
      const conditions = [
        eb("instrument_id", "=", query.instrumentId),
        eb("open_time", ">=", query.from),
        eb("open_time", "<", query.to),
      ];
      if (after) {
        conditions.push(eb("open_time", ">", after));
      }
      return eb.and(conditions);
    })
    .orderBy("open_time", "asc")
    .limit(limit + 1)
    .select([
      "instrument_id",
      "open_time",
      "open",
      "high",
      "low",
      "close",
      "volume",
      "quote_turnover",
      "source",
    ])
    .execute();

  const hasMore = rows.length > limit;
  const page = hasMore ? rows.slice(0, limit) : rows;
  const candles: CandleRow[] = page.map((row) => ({
    instrumentId: row.instrument_id,
    openTime: row.open_time,
    openTimeUnix: Math.floor(row.open_time.getTime() / 1000),
    open: row.open,
    high: row.high,
    low: row.low,
    close: row.close,
    volume: row.volume,
    quoteTurnover: row.quote_turnover,
    source: row.source,
  }));
  const last = page.at(-1);
  return {
    candles,
    nextCursorUnix: hasMore && last ? Math.floor(last.open_time.getTime() / 1000) : null,
  };
}
