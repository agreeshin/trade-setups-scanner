import { type Kysely } from "kysely";
import type { MarketDataDB } from "../db/schema.js";
import { insertCandles } from "../storage/candles.js";
import { getCoverage } from "../storage/coverage.js";
import { getNoTradeRanges, insertNoTradeVerification, subtractRanges, uncoveredRanges } from "../storage/verifications.js";
import type { BybitClient } from "./bybit.js";

const MINUTE_MS = 60_000;
/** Per-run budget of queried minutes; unfinished gaps continue on the next drain call. */
const REPAIR_BUDGET_MINUTES = 200_000;
/** Sanity window queried before repairing to prove the exchange serves this symbol. */
const SANITY_WINDOW_MINUTES = 720;

export interface GapRepairDeps {
  db: Kysely<MarketDataDB>;
  client: BybitClient;
}

export async function createGapRepairJobs(db: Kysely<MarketDataDB>, instrumentIds: number[]): Promise<number[]> {
  if (instrumentIds.length === 0) return [];
  return db.transaction().execute(async (tx) => {
    const ids: number[] = [];
    for (const instrumentId of instrumentIds) {
      const row = await tx
        .insertInto("gap_repair_jobs")
        .values({ instrument_id: instrumentId, status: "queued" })
        .returning("id")
        .executeTakeFirstOrThrow();
      ids.push(row.id);
    }
    return ids;
  });
}

export interface GapRepairJobRow {
  id: number;
  instrumentId: number;
  symbol: string;
  status: string;
  gapsProcessed: number;
  filledMinutes: number;
  verifiedMinutes: number;
  error: string | null;
  createdAt: Date;
  updatedAt: Date;
}

export async function listGapRepairJobs(db: Kysely<MarketDataDB>, limit: number): Promise<GapRepairJobRow[]> {
  return db.selectFrom("gap_repair_jobs")
    .innerJoin("instruments", "instruments.id", "gap_repair_jobs.instrument_id")
    .select([
      "gap_repair_jobs.id as id",
      "gap_repair_jobs.instrument_id as instrumentId",
      "instruments.symbol as symbol",
      "gap_repair_jobs.status as status",
      "gap_repair_jobs.gaps_processed as gapsProcessed",
      "gap_repair_jobs.filled_minutes as filledMinutes",
      "gap_repair_jobs.verified_minutes as verifiedMinutes",
      "gap_repair_jobs.error as error",
      "gap_repair_jobs.created_at as createdAt",
      "gap_repair_jobs.updated_at as updatedAt",
    ])
    .orderBy("gap_repair_jobs.id", "desc")
    .limit(limit)
    .execute();
}

/** Processes queued/running repair jobs one at a time within a wall-clock budget. */
export async function drainGapRepairs(deps: GapRepairDeps, budgetMs = 25_000): Promise<number> {
  const deadline = Date.now() + budgetMs;
  let processed = 0;
  while (Date.now() < deadline) {
    const worked = await processOneGapRepair(deps);
    if (!worked) break;
    processed += 1;
  }
  return processed;
}

async function processOneGapRepair(deps: GapRepairDeps): Promise<boolean> {
  const { db, client } = deps;
  const job = await db.selectFrom("gap_repair_jobs")
    .where("status", "in", ["queued", "running"])
    .orderBy("id", "asc")
    .limit(1)
    .selectAll()
    .executeTakeFirst();
  if (!job) return false;
  await db.updateTable("gap_repair_jobs").set({ status: "running", updated_at: new Date() }).where("id", "=", job.id).execute();

  try {
    const instrument = await db.selectFrom("instruments")
      .innerJoin("exchanges", "exchanges.id", "instruments.exchange_id")
      .where("instruments.id", "=", job.instrument_id)
      .select(["exchanges.code as exchange", "instruments.symbol as symbol"])
      .executeTakeFirstOrThrow();
    const coverage = await getCoverage(db, job.instrument_id);
    if (coverage.firstMinute === null || coverage.lastMinute === null) {
      throw new Error("no stored history to anchor the repair");
    }
    const firstMs = new Date(coverage.firstMinute).getTime();
    const lastMs = new Date(coverage.lastMinute).getTime();

    // Sanity: the exchange must serve a range we already store; an unavailable
    // or delisted symbol must never have ranges marked as no-trade.
    const sanityEndMs = Math.min(firstMs + (SANITY_WINDOW_MINUTES - 1) * MINUTE_MS, lastMs);
    const sanityBars = await client.fetchClosedBars(instrument.symbol, firstMs, sanityEndMs);
    if (sanityBars.length === 0) {
      throw new Error("exchange returned no candles for a range already stored; symbol unavailable");
    }

    // Only gaps between stored candles are repaired: history before our first
    // candle and the tail after the last candle stay out of scope (PRD 7.5/7.6).
    const verified = await getNoTradeRanges(db, job.instrument_id, new Date(firstMs), new Date(lastMs));
    const pending = subtractRanges(
      coverage.gaps.map((gap) => ({ fromMs: new Date(gap.start).getTime(), toMs: new Date(gap.end).getTime() })),
      verified,
    );

    let budget = REPAIR_BUDGET_MINUTES;
    let filled = job.filled_minutes;
    let verifiedMinutes = job.verified_minutes;
    let gapsProcessed = job.gaps_processed;
    let exhausted = false;

    for (const gap of pending) {
      if (budget <= 0) {
        exhausted = true;
        break;
      }
      const gapMinutes = (gap.toMs - gap.fromMs) / MINUTE_MS + 1;
      const endMs = gapMinutes > budget ? gap.fromMs + (budget - 1) * MINUTE_MS : gap.toMs;
      const bars = await client.fetchClosedBars(instrument.symbol, gap.fromMs, endMs);
      await insertCandles(db, job.instrument_id, bars.map((bar) => ({
        openTime: bar.openTime,
        open: bar.open,
        high: bar.high,
        low: bar.low,
        close: bar.close,
        volume: bar.volume,
        quoteTurnover: bar.quoteTurnover,
      })));
      filled += bars.length;
      for (const range of uncoveredRanges(gap.fromMs, endMs, bars.map((bar) => bar.openTime))) {
        await insertNoTradeVerification(db, {
          instrumentId: job.instrument_id,
          fromTs: new Date(range.fromMs),
          toTs: new Date(range.toMs),
          source: "repair",
        });
        verifiedMinutes += (range.toMs - range.fromMs) / MINUTE_MS + 1;
      }
      gapsProcessed += 1;
      budget -= (endMs - gap.fromMs) / MINUTE_MS + 1;
      if (endMs < gap.toMs) {
        exhausted = true;
        break;
      }
    }

    await db.updateTable("gap_repair_jobs").set({
      status: exhausted ? "running" : "done",
      gaps_processed: gapsProcessed,
      filled_minutes: filled,
      verified_minutes: verifiedMinutes,
      error: null,
      updated_at: new Date(),
    }).where("id", "=", job.id).execute();
    return true;
  } catch (error) {
    await db.updateTable("gap_repair_jobs").set({
      status: "failed",
      error: error instanceof Error ? error.message : String(error),
      updated_at: new Date(),
    }).where("id", "=", job.id).execute();
    return true;
  }
}
