import { sql, type Kysely } from "kysely";
import type { MarketDataDB } from "../db/schema.js";

/** Bootstraps old databases lazily; one indexed latest-row lookup per new ID. */
export async function ensureWatermarks(db: Kysely<MarketDataDB>, ids: number[]): Promise<void> {
  if (ids.length === 0) return;
  await sql`
    INSERT INTO instrument_watermarks (instrument_id, latest_open_time, latest_received_at)
    SELECT i.id, c.open_time, c.ingested_at FROM (
      SELECT id FROM instruments WHERE id = ANY(${ids}::int[]) AND NOT EXISTS
        (SELECT 1 FROM instrument_watermarks w WHERE w.instrument_id = instruments.id)
      OFFSET 0
    ) i
    LEFT JOIN LATERAL (SELECT open_time, ingested_at FROM candles WHERE instrument_id = i.id
      ORDER BY open_time DESC LIMIT 1) c ON true
    ON CONFLICT (instrument_id) DO NOTHING
  `.execute(db);
}

/** Must run before subscribing: a newer WS candle must not hide an older tail. */
export async function initializeCollectionTails(db: Kysely<MarketDataDB>, ids: number[]): Promise<void> {
  if (ids.length === 0) return;
  await ensureWatermarks(db, ids);
  await sql`UPDATE collection_targets t SET tail_initialized = true,
    tail_next_ts = w.latest_open_time + interval '1 minute'
    FROM instrument_watermarks w WHERE t.instrument_id = w.instrument_id
      AND t.instrument_id = ANY(${ids}::int[]) AND NOT t.tail_initialized`.execute(db);
}
