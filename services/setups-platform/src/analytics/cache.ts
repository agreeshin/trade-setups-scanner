import { type Kysely, sql } from "kysely";
import type { SignalsDB } from "../db/schema.js";

/**
 * Analytics result cache stored in the platform (signals) DB. Key design
 * follows the reference project: the setup-set fingerprint (count, first
 * and last signal, last available candle of the last setup window) plus
 * every computation parameter. `-1` sentinels mean "parameter not set"
 * for maturity tenths and price-change ppm; a data revision inside an
 * unchanged fingerprint is not detected (explicit clear + recompute).
 */
export const ANALYTICS_CACHE_ALGORITHM_VERSION = 1;

export type Direction = "short" | "long";

export interface SetScope {
  setKind: string;
  setId: number;
  setupCount: number;
  firstSetupUnix: number | null;
  lastSetupUnix: number | null;
  /** Last candle open time in the window of the last setup; -1 when none. */
  lastSetupLastCandleOpenTimeUnix: number;
}

export interface ComputeParams {
  direction: Direction;
  tpPct: number;
  slPct: number;
  horizonMinutes: number;
  timeShiftMinutes: number;
  /** Normalized to millionths of a percent; -1 = gate disabled. */
  priceChangePpm: number;
  /** Maturity in tenths; -1 = no maturity clustering. */
  maturityTenths: number;
  /** Cluster gap in minutes; -1 = clustering disabled. */
  clusterGapMinutes: number;
}

export function priceChangePpmOf(pct: number | null | undefined): number {
  if (pct === null || pct === undefined) return -1;
  return Math.round(pct * 10_000);
}

export function maturityTenthsOf(maturity: number | null | undefined): number {
  if (maturity === null || maturity === undefined) return -1;
  return Math.round(maturity * 10);
}

export function matrixScopeKey(scope: SetScope): string {
  return [
    scope.setKind, scope.setId, scope.setupCount,
    scope.firstSetupUnix ?? -1, scope.lastSetupUnix ?? -1,
    scope.lastSetupLastCandleOpenTimeUnix,
  ].join(":");
}

export function selectionScopeKey(scope: Omit<SetScope, "lastSetupLastCandleOpenTimeUnix">): string {
  return [scope.setKind, scope.setId, scope.setupCount, scope.firstSetupUnix ?? -1, scope.lastSetupUnix ?? -1].join(":");
}

export type CachedPayload = Record<string, unknown>;

export async function getMatrixCell(
  db: Kysely<SignalsDB>,
  scopeKey: string,
  params: ComputeParams,
  monthKey: string,
): Promise<CachedPayload | undefined> {
  const row = await db
    .selectFrom("analytics_matrix_cache")
    .select("result_json")
    .where("scope_key", "=", scopeKey)
    .where("direction", "=", params.direction)
    .where("maturity_tenths", "=", params.maturityTenths)
    .where("cluster_gap_minutes", "=", params.clusterGapMinutes)
    .where("month_key", "=", monthKey)
    .where("tp_pct", "=", params.tpPct.toString())
    .where("sl_pct", "=", params.slPct.toString())
    .where("horizon_minutes", "=", params.horizonMinutes)
    .where("time_shift_minutes", "=", params.timeShiftMinutes)
    .where("price_change_ppm", "=", params.priceChangePpm)
    .where("algorithm_version", "=", ANALYTICS_CACHE_ALGORITHM_VERSION)
    .executeTakeFirst();
  return row === undefined ? undefined : row.result_json;
}

export async function saveMatrixCell(
  db: Kysely<SignalsDB>,
  scopeKey: string,
  params: ComputeParams,
  monthKey: string,
  result: CachedPayload,
): Promise<void> {
  await db
    .insertInto("analytics_matrix_cache")
    .values({
      scope_key: scopeKey,
      direction: params.direction,
      maturity_tenths: params.maturityTenths,
      cluster_gap_minutes: params.clusterGapMinutes,
      month_key: monthKey,
      tp_pct: params.tpPct.toString(),
      sl_pct: params.slPct.toString(),
      horizon_minutes: params.horizonMinutes,
      time_shift_minutes: params.timeShiftMinutes,
      price_change_ppm: params.priceChangePpm,
      algorithm_version: ANALYTICS_CACHE_ALGORITHM_VERSION,
      result_json: result,
    })
    .onConflict((b) => b.columns(["scope_key", "direction", "maturity_tenths", "cluster_gap_minutes", "month_key", "tp_pct", "sl_pct", "horizon_minutes", "time_shift_minutes", "price_change_ppm", "algorithm_version"])
      .doUpdateSet({ result_json: result, created_at: sql`now()` }))
    .execute();
}

/** Cache-only bulk read of every stored cell for the scope (any TP/SL). */
export async function listMatrixCells(
  db: Kysely<SignalsDB>,
  scopeKey: string,
  params: Omit<ComputeParams, "tpPct" | "slPct">,
  monthKey: string,
): Promise<{ tpPct: number; slPct: number; payload: CachedPayload }[]> {
  const rows = await db
    .selectFrom("analytics_matrix_cache")
    .select(["tp_pct", "sl_pct", "result_json"])
    .where("scope_key", "=", scopeKey)
    .where("direction", "=", params.direction)
    .where("maturity_tenths", "=", params.maturityTenths)
    .where("cluster_gap_minutes", "=", params.clusterGapMinutes)
    .where("month_key", "=", monthKey)
    .where("horizon_minutes", "=", params.horizonMinutes)
    .where("time_shift_minutes", "=", params.timeShiftMinutes)
    .where("price_change_ppm", "=", params.priceChangePpm)
    .where("algorithm_version", "=", ANALYTICS_CACHE_ALGORITHM_VERSION)
    .execute();
  return rows.map((row) => ({ tpPct: Number(row.tp_pct), slPct: Number(row.sl_pct), payload: row.result_json }));
}

export interface TradeSetupIdentity {
  exchangeCode: string;
  category: string;
  symbol: string;
  closeTimeUnix: number;
}

export async function getTradeRow(
  db: Kysely<SignalsDB>,
  scopeKey: string,
  params: ComputeParams,
  identity: TradeSetupIdentity,
): Promise<CachedPayload | undefined> {
  const row = await db
    .selectFrom("analytics_trades_cache")
    .select("result_json")
    .where("scope_key", "=", scopeKey)
    .where("direction", "=", params.direction)
    .where("maturity_tenths", "=", params.maturityTenths)
    .where("cluster_gap_minutes", "=", params.clusterGapMinutes)
    .where("exchange_code", "=", identity.exchangeCode)
    .where("category", "=", identity.category)
    .where("symbol", "=", identity.symbol)
    .where("close_time_unix", "=", String(identity.closeTimeUnix))
    .where("tp_pct", "=", params.tpPct.toString())
    .where("sl_pct", "=", params.slPct.toString())
    .where("horizon_minutes", "=", params.horizonMinutes)
    .where("time_shift_minutes", "=", params.timeShiftMinutes)
    .where("price_change_ppm", "=", params.priceChangePpm)
    .where("algorithm_version", "=", ANALYTICS_CACHE_ALGORITHM_VERSION)
    .executeTakeFirst();
  return row === undefined ? undefined : row.result_json;
}

export async function saveTradeRow(
  db: Kysely<SignalsDB>,
  scopeKey: string,
  params: ComputeParams,
  identity: TradeSetupIdentity,
  result: CachedPayload,
): Promise<void> {
  await db
    .insertInto("analytics_trades_cache")
    .values({
      scope_key: scopeKey,
      direction: params.direction,
      maturity_tenths: params.maturityTenths,
      cluster_gap_minutes: params.clusterGapMinutes,
      exchange_code: identity.exchangeCode,
      category: identity.category,
      symbol: identity.symbol,
      close_time_unix: String(identity.closeTimeUnix),
      tp_pct: params.tpPct.toString(),
      sl_pct: params.slPct.toString(),
      horizon_minutes: params.horizonMinutes,
      time_shift_minutes: params.timeShiftMinutes,
      price_change_ppm: params.priceChangePpm,
      algorithm_version: ANALYTICS_CACHE_ALGORITHM_VERSION,
      result_json: result,
    })
    .onConflict((b) => b.columns(["scope_key", "direction", "maturity_tenths", "cluster_gap_minutes", "exchange_code", "category", "symbol", "close_time_unix", "tp_pct", "sl_pct", "horizon_minutes", "time_shift_minutes", "price_change_ppm", "algorithm_version"])
      .doUpdateSet({ result_json: result, created_at: sql`now()` }))
    .execute();
}

/** Cache-only bulk read of every stored row for the scope. */
export async function listTradeRows(
  db: Kysely<SignalsDB>,
  scopeKey: string,
  params: ComputeParams,
): Promise<{ identity: TradeSetupIdentity; payload: CachedPayload }[]> {
  const rows = await db
    .selectFrom("analytics_trades_cache")
    .select(["exchange_code", "category", "symbol", "close_time_unix", "result_json"])
    .where("scope_key", "=", scopeKey)
    .where("direction", "=", params.direction)
    .where("maturity_tenths", "=", params.maturityTenths)
    .where("cluster_gap_minutes", "=", params.clusterGapMinutes)
    .where("tp_pct", "=", params.tpPct.toString())
    .where("sl_pct", "=", params.slPct.toString())
    .where("horizon_minutes", "=", params.horizonMinutes)
    .where("time_shift_minutes", "=", params.timeShiftMinutes)
    .where("price_change_ppm", "=", params.priceChangePpm)
    .where("algorithm_version", "=", ANALYTICS_CACHE_ALGORITHM_VERSION)
    .orderBy("close_time_unix", "asc")
    .execute();
  return rows.map((row) => ({
    identity: {
      exchangeCode: row.exchange_code,
      category: row.category,
      symbol: row.symbol,
      closeTimeUnix: Number(row.close_time_unix),
    },
    payload: row.result_json,
  }));
}

export async function getSelection(
  db: Kysely<SignalsDB>,
  scopeKey: string,
  maturityTenths: number,
  clusterGapMinutes: number,
  monthKey: string,
): Promise<CachedPayload | undefined> {
  const row = await db
    .selectFrom("analytics_selection_cache")
    .select("result_json")
    .where("scope_key", "=", scopeKey)
    .where("maturity_tenths", "=", maturityTenths)
    .where("cluster_gap_minutes", "=", clusterGapMinutes)
    .where("month_key", "=", monthKey)
    .where("algorithm_version", "=", ANALYTICS_CACHE_ALGORITHM_VERSION)
    .executeTakeFirst();
  return row === undefined ? undefined : row.result_json;
}

export async function saveSelection(
  db: Kysely<SignalsDB>,
  scopeKey: string,
  maturityTenths: number,
  clusterGapMinutes: number,
  monthKey: string,
  result: CachedPayload,
): Promise<void> {
  await db
    .insertInto("analytics_selection_cache")
    .values({
      scope_key: scopeKey,
      maturity_tenths: maturityTenths,
      cluster_gap_minutes: clusterGapMinutes,
      month_key: monthKey,
      algorithm_version: ANALYTICS_CACHE_ALGORITHM_VERSION,
      result_json: result,
    })
    .onConflict((b) => b.columns(["scope_key", "maturity_tenths", "cluster_gap_minutes", "month_key", "algorithm_version"])
      .doUpdateSet({ result_json: result, created_at: sql`now()` }))
    .execute();
}

export async function analyticsCacheSummary(db: Kysely<SignalsDB>): Promise<{ tables: { name: string; count: number }[]; total: number }> {
  const names = ["analytics_matrix_cache", "analytics_trades_cache", "analytics_selection_cache"] as const;
  const tables: { name: string; count: number }[] = [];
  let total = 0;
  for (const name of names) {
    const result = await sql<{ count: string | number }>`select count(*) as count from ${sql.table(name)}`.execute(db);
    const count = Number(result.rows[0]?.count ?? 0);
    tables.push({ name, count });
    total += count;
  }
  return { tables, total };
}

/** Clear analytics cache tables only; setups, runs and replay cache stay. */
export async function clearAnalyticsCache(db: Kysely<SignalsDB>): Promise<{ deleted: { name: string; count: number }[]; total: number }> {
  const names = ["analytics_matrix_cache", "analytics_trades_cache", "analytics_selection_cache"] as const;
  const deleted: { name: string; count: number }[] = [];
  let total = 0;
  for (const name of names) {
    const result = await db.deleteFrom(name).returningAll().execute();
    deleted.push({ name, count: result.length });
    total += result.length;
  }
  return { deleted, total };
}
