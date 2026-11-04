import type { Kysely } from "kysely";
import type { MarketDataDB, CandleRecordJson } from "../db/schema.js";
import { ensureMonthPartitions } from "./partitions.js";

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
  await ensureMonthPartitions(
    db,
    new Date(Math.min(...times)),
    new Date(Math.max(...times)),
  );

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

  await db
    .insertInto("candles")
    .values(uniqueCandles.map((candle) => candleValues(instrumentId, candle)))
    .onConflict((oc) => oc.constraint("candles_pkey").doNothing())
    .execute();

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

  return {
    inserted: uniqueCandles.length - existingByTime.size,
    conflicting: conflicts.length,
  };
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
