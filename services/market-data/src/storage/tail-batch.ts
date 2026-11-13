import { sql, type Kysely } from "kysely";
import type { MarketDataDB } from "../db/schema.js";
import { readGuards, type ReadGuardInput } from "./read-guards.js";

export const TAIL_MAX_BYTES = 8 * 1024 * 1024;
export class TailBatchSizeError extends Error {}
export interface TailBatchInput {
  instrumentIds: number[];
  fromUnix: number;
  toUnix: number;
  readGuards?: ReadGuardInput[];
}

export async function readTailBatch(db: Kysely<MarketDataDB>, input: TailBatchInput) {
  return db.transaction().setIsolationLevel("repeatable read").execute(async (tx) => {
    const from = new Date(input.fromUnix * 1000);
    const to = new Date(input.toUnix * 1000);
    // Guard before decimal strings reach Node, including old imported numerics
    // larger than the current source adapter accepts. No history-wide scans.
    const size = await sql<{ bytes: string }>`SELECT (coalesce(sum(
      octet_length(open::text) + octet_length(high::text) + octet_length(low::text) +
      octet_length(close::text) + octet_length(volume::text) + octet_length(quote_turnover::text) +
      octet_length(to_json(source)::text) + 256), 0) + 262144)::text AS bytes
      FROM candles WHERE instrument_id = ANY(${input.instrumentIds}::int[])
        AND open_time >= ${from} AND open_time < ${to}`.execute(tx);
    if (Number(size.rows[0]!.bytes) > TAIL_MAX_BYTES) throw new TailBatchSizeError("tail response exceeds byte limit");
    const metadata = await sql<{ id: number; latest: Date | null; checked: Date }>`
      SELECT i.id, coalesce(w.latest_open_time, c.open_time) AS latest, statement_timestamp() AS checked
      FROM instruments i LEFT JOIN instrument_watermarks w ON w.instrument_id = i.id
      LEFT JOIN LATERAL (SELECT open_time FROM candles WHERE instrument_id = i.id
        AND w.latest_open_time IS NULL ORDER BY open_time DESC LIMIT 1) c ON true
      WHERE i.id = ANY(${input.instrumentIds}::int[])`.execute(tx);
    const rows = await tx.selectFrom("candles").selectAll()
      .where("instrument_id", "in", input.instrumentIds).where("open_time", ">=", from).where("open_time", "<", to)
      .orderBy("instrument_id").orderBy("open_time").limit(6001).execute();
    if (rows.length > 6000) throw new TailBatchSizeError("tail response exceeds item limit");
    const byId = new Map(metadata.rows.map((row) => [row.id, row]));
    const guards = await readGuards(tx, input.readGuards ?? []);
    const candlesById = new Map<number, typeof rows>();
    for (const row of rows) {
      const candles = candlesById.get(row.instrument_id) ?? [];
      candles.push(row); candlesById.set(row.instrument_id, candles);
    }
    const body = {
      checkedAt: (metadata.rows[0]?.checked ?? new Date()).toISOString(),
      instruments: input.instrumentIds.map((instrumentId) => {
        const info = byId.get(instrumentId);
        const candles = (candlesById.get(instrumentId) ?? []).map((row) => ({
          instrumentId, openTimeUnix: row.open_time.getTime() / 1000,
          open: row.open, high: row.high, low: row.low, close: row.close,
          volume: row.volume, quoteTurnover: row.quote_turnover, source: row.source,
        }));
        const missingRanges: { fromUnix: number; toUnix: number }[] = [];
        let cursor = input.fromUnix;
        let contiguousThroughOpenTimeUnix: number | null = null;
        for (const candle of candles) {
          if (candle.openTimeUnix > cursor) missingRanges.push({ fromUnix: cursor, toUnix: candle.openTimeUnix });
          if (missingRanges.length === 0) contiguousThroughOpenTimeUnix = candle.openTimeUnix;
          cursor = candle.openTimeUnix + 60;
        }
        if (cursor < input.toUnix) missingRanges.push({ fromUnix: cursor, toUnix: input.toUnix });
        return { instrumentId, status: info ? "known" as const : "not_found" as const,
          latestStoredOpenTimeUnix: info?.latest ? info.latest.getTime() / 1000 : null,
          contiguousThroughOpenTimeUnix, candles, missingRanges,
          ...(guards.has(instrumentId) ? { readGuard: guards.get(instrumentId)! } : {}),
        };
      }),
    };
    if (Buffer.byteLength(JSON.stringify(body)) > TAIL_MAX_BYTES) throw new TailBatchSizeError("tail response exceeds byte limit");
    return body;
  });
}
