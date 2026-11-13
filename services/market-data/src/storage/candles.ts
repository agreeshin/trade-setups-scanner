import { sql, type Kysely } from "kysely";
import type { MarketDataDB, CandleRecordJson } from "../db/schema.js";
import { ensureMonthPartitions } from "./partitions.js";
import { ensureWatermarks } from "./watermarks.js";
import { recordCanonicalInsert } from "./read-guards.js";

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
  const trimmed = value.trim();
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
  if (candles.length === 0) {
    return { inserted: 0, conflicting: 0 };
  }

  const uniqueByTime = new Map<number, CandleInsert>();
  for (const candle of candles) {
    uniqueByTime.set(candle.openTime.getTime(), candle);
  }
  const uniqueCandles = [...uniqueByTime.values()];

  const times = uniqueCandles.map((candle) => candle.openTime.getTime());
  await ensureWatermarks(db, [instrumentId]);
  await ensureMonthPartitions(
    db,
    new Date(Math.min(...times)),
    new Date(Math.max(...times)),
  );

  const write = async (db: Kysely<MarketDataDB>): Promise<InsertResult> => {
  // Serialize BEFORE any canonical insertion. Revision allocation after an
  // unlocked insert would permit commit-order gaps across concurrent writers.
  await db.selectFrom("instrument_watermarks").where("instrument_id", "=", instrumentId)
    .select("instrument_id").forUpdate().executeTakeFirstOrThrow();
  const journal = await db.selectFrom("candle_insert_heads").where("instrument_id", "=", instrumentId)
    .select("instrument_id").executeTakeFirst();
  if (!journal) throw new Error(`canonical insert metadata missing for instrument ${instrumentId}`);
  const inserted = await db
    .insertInto("candles")
    .values(uniqueCandles.map((candle) => candleValues(instrumentId, candle)))
    .onConflict((oc) => oc.constraint("candles_pkey").doNothing())
    .returning("open_time")
    .execute();
  await recordCanonicalInsert(db, instrumentId, inserted.map((row) => row.open_time));

  // Read after ON CONFLICT has waited for concurrent writers. Comparing a
  // pre-insert snapshot would miss WS/REST conflicts under READ COMMITTED.
  const existingRows = await db
    .selectFrom("candles")
    .where((eb) =>
      eb.and([
        eb("instrument_id", "=", instrumentId),
        eb(
          "open_time",
          "in",
          uniqueCandles.map((candle) => candle.openTime),
        ),
      ]),
    )
    .select([
      "open_time",
      "open",
      "high",
      "low",
      "close",
      "volume",
      "quote_turnover",
    ])
    .execute();

  const existingByTime = new Map(
    existingRows.map((row) => [row.open_time.getTime(), row]),
  );

  const conflicts = uniqueCandles.filter((candle) => {
    const existing = existingByTime.get(candle.openTime.getTime());
    return existing !== undefined && differsFromStored(existing, candle);
  });

  if (conflicts.length > 0) {
    await db
      .insertInto("candle_conflicts")
      .values(
        conflicts.map((candle) => {
          const existing = existingByTime.get(candle.openTime.getTime());
          if (existing === undefined) {
            throw new Error("conflict row missing existing record");
          }
          return {
            instrument_id: instrumentId,
            open_time: candle.openTime,
            existing_record: toConflictJson(existing),
            incoming_record: toConflictJson({
              open: candle.open,
              high: candle.high,
              low: candle.low,
              close: candle.close,
              volume: candle.volume,
              quote_turnover: candle.quoteTurnover,
            }),
          };
        }),
      )
      .onConflict((oc) => oc.constraint("candle_conflicts_unique").doNothing())
      .execute();
  }

  const latest = new Date(Math.max(...times));
  await sql`INSERT INTO instrument_watermarks (instrument_id, latest_open_time, latest_received_at)
    VALUES (${instrumentId}, ${latest}, statement_timestamp())
    ON CONFLICT (instrument_id) DO UPDATE SET
      latest_open_time = greatest(instrument_watermarks.latest_open_time, excluded.latest_open_time),
      latest_received_at = CASE WHEN instrument_watermarks.latest_open_time IS NULL
        OR excluded.latest_open_time > instrument_watermarks.latest_open_time
        THEN excluded.latest_received_at ELSE instrument_watermarks.latest_received_at END`.execute(db);
  await sql`UPDATE collection_targets SET last_collected_ts = greatest(last_collected_ts, ${latest})
    WHERE instrument_id = ${instrumentId}`.execute(db);
  return {
    inserted: inserted.length,
    conflicting: conflicts.length,
  };
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
