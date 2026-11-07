import type { Kysely } from "kysely";
import type { MarketDataDB } from "../db/schema.js";

const MINUTE_MS = 60_000;

export interface MinuteRange {
  fromMs: number;
  toMs: number;
}

export interface NoTradeVerificationInput {
  instrumentId: number;
  fromTs: Date;
  toTs: Date;
  source: "download" | "repair";
}

export async function insertNoTradeVerification(
  db: Kysely<MarketDataDB>,
  entry: NoTradeVerificationInput,
): Promise<void> {
  if (entry.toTs.getTime() < entry.fromTs.getTime()) return;
  await db
    .insertInto("gap_verifications")
    .values({
      instrument_id: entry.instrumentId,
      from_ts: entry.fromTs,
      to_ts: entry.toTs,
      kind: "no_trade",
      source: entry.source,
    })
    .execute();
}

/** Verified no-trade ranges that intersect [fromTs, toTs]. */
export async function getNoTradeRanges(
  db: Kysely<MarketDataDB>,
  instrumentId: number,
  fromTs: Date,
  toTs: Date,
): Promise<MinuteRange[]> {
  const rows = await db
    .selectFrom("gap_verifications")
    .select(["from_ts", "to_ts"])
    .where("instrument_id", "=", instrumentId)
    .where("kind", "=", "no_trade")
    .where("from_ts", "<=", toTs)
    .where("to_ts", ">=", fromTs)
    .orderBy("from_ts", "asc")
    .execute();
  return rows.map((row) => ({ fromMs: row.from_ts.getTime(), toMs: row.to_ts.getTime() }));
}

/** Complement of covered minute open-times within [fromMs, toMs], minute-aligned. */
export function uncoveredRanges(fromMs: number, toMs: number, coveredOpenTimes: Date[]): MinuteRange[] {
  const covered = [...new Set(coveredOpenTimes.map((time) => time.getTime()))].sort((a, b) => a - b);
  const ranges: MinuteRange[] = [];
  let cursor = fromMs;
  for (const minute of covered) {
    if (minute < cursor) continue;
    if (minute > toMs) break;
    if (minute > cursor) ranges.push({ fromMs: cursor, toMs: minute - MINUTE_MS });
    cursor = minute + MINUTE_MS;
  }
  if (cursor <= toMs) ranges.push({ fromMs: cursor, toMs });
  return ranges.filter((range) => range.toMs >= range.fromMs);
}

/** Removes `remove` ranges from `targets`; returns the disjoint remainder. */
export function subtractRanges(targets: MinuteRange[], remove: MinuteRange[]): MinuteRange[] {
  const result: MinuteRange[] = [];
  for (const target of [...targets].sort((a, b) => a.fromMs - b.fromMs)) {
    let pieces: MinuteRange[] = [target];
    for (const cut of remove) {
      const next: MinuteRange[] = [];
      for (const piece of pieces) {
        if (cut.toMs < piece.fromMs || cut.fromMs > piece.toMs) {
          next.push(piece);
          continue;
        }
        if (cut.fromMs > piece.fromMs) {
          next.push({ fromMs: piece.fromMs, toMs: Math.min(cut.fromMs - MINUTE_MS, piece.toMs) });
        }
        if (cut.toMs < piece.toMs) {
          next.push({ fromMs: Math.max(cut.toMs + MINUTE_MS, piece.fromMs), toMs: piece.toMs });
        }
      }
      pieces = next.filter((piece) => piece.toMs >= piece.fromMs);
    }
    result.push(...pieces);
  }
  return result;
}
