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

export const COVERAGE_MIN_UNIX = -62_135_596_800;
export const COVERAGE_MAX_UNIX = 253_402_300_740;

export interface CoverageRangeInput {
  fromUnix: number;
  toUnix: number;
  splitUnix: number;
  offset?: number;
  limit?: number;
}

export interface CoverageRange {
  instrumentId: number;
  fromUnix: number;
  toUnix: number;
  splitUnix: number;
  expectedMinutes: number;
  presentMinutes: number;
  missingMinutes: number;
  warmupExpectedMinutes: number;
  warmupPresentMinutes: number;
  signalExpectedMinutes: number;
  signalPresentMinutes: number;
  gaps: { fromUnix: number; toUnix: number }[];
  gapCount: number;
  offset: number;
  limit: number;
  checkedAt: string;
}

/** One statement snapshot; only aggregate values and the bounded gap page reach Node. */
export async function getCoverageRange(
  db: Kysely<MarketDataDB>, instrumentId: number, input: CoverageRangeInput,
): Promise<CoverageRange | null> {
  const fromUnix = Math.ceil(input.fromUnix / 60) * 60;
  const toUnix = Math.ceil(input.toUnix / 60) * 60;
  const splitUnix = Math.ceil(input.splitUnix / 60) * 60;
  const offset = input.offset ?? 0;
  const limit = input.limit ?? 50;
  const result = await sql<{
    presentMinutes: number;
    warmupPresentMinutes: number;
    signalPresentMinutes: number;
    gapCount: number;
    gaps: CoverageRange["gaps"];
    checkedAt: Date;
  }>`
    WITH points AS MATERIALIZED (
      SELECT extract(epoch FROM open_time)::bigint AS minute
      FROM candles
      WHERE instrument_id = ${instrumentId}
        AND open_time >= to_timestamp(${fromUnix})
        AND open_time < to_timestamp(${toUnix})
        AND mod(extract(epoch FROM open_time), 60) = 0
    ), ordered AS (
      SELECT minute, lag(minute) OVER (ORDER BY minute) AS previous FROM points
    ), summary AS (
      SELECT count(*)::double precision AS present,
        (count(*) FILTER (WHERE minute < ${splitUnix}))::double precision AS warmup,
        (count(*) FILTER (WHERE minute >= ${splitUnix}))::double precision AS signal,
        max(minute) AS last_minute
      FROM points
    ), gaps AS (
      SELECT coalesce(previous + 60, ${fromUnix}) AS gap_from, minute AS gap_to
      FROM ordered WHERE minute > coalesce(previous + 60, ${fromUnix})
      UNION ALL
      SELECT coalesce(last_minute + 60, ${fromUnix}), ${toUnix}::bigint
      FROM summary WHERE coalesce(last_minute + 60, ${fromUnix}) < ${toUnix}
    ), page AS (
      SELECT gap_from, gap_to FROM gaps ORDER BY gap_from LIMIT ${limit} OFFSET ${offset}
    )
    SELECT present AS "presentMinutes", warmup AS "warmupPresentMinutes", signal AS "signalPresentMinutes",
      (SELECT count(*)::double precision FROM gaps) AS "gapCount",
      coalesce((SELECT jsonb_agg(jsonb_build_object('fromUnix', gap_from, 'toUnix', gap_to) ORDER BY gap_from) FROM page), '[]'::jsonb) AS gaps,
      statement_timestamp() AS "checkedAt"
    FROM summary WHERE EXISTS (SELECT 1 FROM instruments WHERE id = ${instrumentId})
  `.execute(db);
  const row = result.rows[0];
  if (!row) return null;
  const expectedMinutes = (toUnix - fromUnix) / 60;
  return {
    instrumentId, fromUnix, toUnix, splitUnix, expectedMinutes,
    presentMinutes: row.presentMinutes, missingMinutes: expectedMinutes - row.presentMinutes,
    warmupExpectedMinutes: (splitUnix - fromUnix) / 60, warmupPresentMinutes: row.warmupPresentMinutes,
    signalExpectedMinutes: (toUnix - splitUnix) / 60, signalPresentMinutes: row.signalPresentMinutes,
    gaps: row.gaps, gapCount: row.gapCount, offset, limit, checkedAt: row.checkedAt.toISOString(),
  };
}

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
