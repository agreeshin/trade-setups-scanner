import { sql, type Kysely } from "kysely";
import type { MarketDataDB } from "../db/schema.js";

export interface IntervalIso {
  start: string;
  end: string;
}

export interface Coverage {
  firstMinute: string | null;
  lastMinute: string | null;
  totalCandles: number;
  intervals: IntervalIso[];
  gaps: IntervalIso[];
}

const MINUTE_MS = 60_000;

export async function getCoverage(
  db: Kysely<MarketDataDB>,
  instrumentId: number,
): Promise<Coverage> {
  const totalRow = await db
    .selectFrom("candles")
    .where("instrument_id", "=", instrumentId)
    .select((eb) => eb.fn.countAll<string>().as("count"))
    .executeTakeFirst();

  const boundaries = await sql<{ open_time: Date; prev: Date | null }>`
    WITH pts AS (
      SELECT open_time, lag(open_time) OVER (ORDER BY open_time) AS prev
      FROM candles WHERE instrument_id = ${instrumentId}
    )
    SELECT open_time, prev FROM pts
    WHERE prev IS NULL OR open_time - prev > interval '1 minute'`.execute(db);

  const total = totalRow ? Number(totalRow.count) : 0;
  if (boundaries.rows.length === 0 || total === 0) {
    return {
      firstMinute: null,
      lastMinute: null,
      totalCandles: 0,
      intervals: [],
      gaps: [],
    };
  }

  const intervals: IntervalIso[] = [];
  const gaps: IntervalIso[] = [];
  let intervalStart: Date = boundaries.rows[0]!.open_time;

  for (const row of boundaries.rows.slice(1)) {
    const previousEnd = row.prev;
    if (previousEnd === null) {
      continue;
    }
    intervals.push({
      start: intervalStart.toISOString(),
      end: previousEnd.toISOString(),
    });
    gaps.push({
      start: new Date(previousEnd.getTime() + MINUTE_MS).toISOString(),
      end: new Date(row.open_time.getTime() - MINUTE_MS).toISOString(),
    });
    intervalStart = row.open_time;
  }

  const lastRow = await sql<{ max: Date }>`
    SELECT max(open_time) AS max FROM candles WHERE instrument_id = ${instrumentId}`.execute(
    db,
  );
  const lastMinute = lastRow.rows[0]!.max;
  intervals.push({
    start: intervalStart.toISOString(),
    end: lastMinute.toISOString(),
  });

  return {
    firstMinute: boundaries.rows[0]!.open_time.toISOString(),
    lastMinute: lastMinute.toISOString(),
    totalCandles: total,
    intervals,
    gaps,
  };
}
