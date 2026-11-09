/**
 * Pure analytics engine: setup clustering by maturity, price-change gate
 * and TP/SL limit evaluation over minute candles.
 *
 * Principles reproduce the owner's reference project (blago_bot_analysis):
 * - aggregates are computed only over closed TP/SL trades;
 * - if one candle touches both TP and SL, SL wins;
 * - month filter applies to the original setup time and never cuts the
 *   trade horizon;
 * - risk expectancy is avg_move_pct / sl_pct (null without closed trades).
 *
 * All times are Unix seconds. Prices arrive as decimal strings from the
 * market-data API and are converted to JS numbers for simulation, exactly
 * like the reference analytics.
 */

export type TradeDirection = "short" | "long";
export type LimitStatus = "tp" | "sl" | "unclosed" | "no_data" | "price_change_filtered";

export const SETUP_CLUSTER_MAX_GAP_SECONDS = 8 * 60 * 60;
export const DEFAULT_CLUSTER_GAP_HOURS = 8;
export const DEFAULT_HORIZON_MINUTES = 10_080;
export const MINUTE_SECONDS = 60;

export interface AnalyticsSetup {
  exchangeCode: string;
  category: string;
  symbol: string;
  /** open time of the signal minute, Unix seconds. */
  openTimeUnix: number;
  /** close time of the signal minute = setup time, Unix seconds. */
  closeTimeUnix: number;
  /** close price of the signal candle as decimal string. */
  closePrice: string;
}

export interface OhlcCandle {
  openTimeUnix: number;
  open: number;
  high: number;
  low: number;
  close: number;
}

export interface LimitEvaluation {
  status: LimitStatus;
  entryPrice: number | null;
  entryTimeUnix: number | null;
  tpPrice: number | null;
  slPrice: number | null;
  closeTimeUnix: number | null;
  /** Signed move in percent: +tpPct / -slPct for closed trades. */
  movePct: number | null;
  /** Minutes between entry and close for closed trades. */
  closeMinutes: number | null;
}

export interface LimitAggregate {
  closedCount: number;
  tpCount: number;
  slCount: number;
  unclosedCount: number;
  noDataCount: number;
  avgMovePct: number | null;
  medianMovePct: number | null;
  winRatePct: number | null;
  avgCloseMinutes: number | null;
  medianCloseMinutes: number | null;
  /** avgMovePct / slPct, null without closed trades. */
  riskExpectancy: number | null;
}

export interface MaturitySelection {
  rows: AnalyticsSetup[];
  avgSelectionOrdinal: number | null;
  avgSelectionDelayMinutes: number | null;
  totalClustersCount: number;
}

const mean = (values: number[]): number | null => (values.length > 0 ? values.reduce((a, b) => a + b, 0) / values.length : null);

function median(values: number[]): number | null {
  if (values.length === 0) return null;
  const sorted = [...values].sort((a, b) => a - b);
  const middle = Math.floor(sorted.length / 2);
  const at = sorted[middle];
  const before = sorted[middle - 1];
  if (at === undefined) return null;
  if (sorted.length % 2 === 1) return at;
  return before === undefined ? at : (before + at) / 2;
}

export function setupInstrumentKey(setup: AnalyticsSetup): string {
  return `${setup.exchangeCode}\u0000${setup.category}\u0000${setup.symbol}`;
}

export function monthKeyOf(unixSeconds: number): string {
  const date = new Date(unixSeconds * 1000);
  const month = String(date.getUTCMonth() + 1).padStart(2, "0");
  return `${date.getUTCFullYear()}-${month}`;
}

export function monthKeyToStartUnix(monthKey: string): number {
  const [year, month] = monthKey.split("-").map(Number);
  if (year === undefined || month === undefined || month < 1 || month > 12) {
    throw new Error(`invalid month key: ${monthKey}`);
  }
  return Math.floor(Date.UTC(year, month - 1, 1) / 1000);
}

/** UTC months (YYYY-MM) that contain at least one setup, ascending. */
export function setupMonths(setups: AnalyticsSetup[]): string[] {
  const months = new Set(setups.map((setup) => monthKeyOf(setup.closeTimeUnix)));
  return [...months].sort();
}

/** Keep setups whose signal time falls into the UTC month. */
export function filterByMonth(setups: AnalyticsSetup[], monthKey: string): AnalyticsSetup[] {
  const start = monthKeyToStartUnix(monthKey);
  const nextMonth = monthKey === "12" ? monthKeyToStartUnix(`${Number(monthKey.slice(0, 4)) + 1}-01`) : monthKeyToStartUnix(`${monthKey.slice(0, 4)}-${String(Number(monthKey.slice(5)) + 1).padStart(2, "0")}`);
  return setups.filter((setup) => setup.closeTimeUnix >= start && setup.closeTimeUnix < nextMonth);
}

/** UTC unix bounds of a day range: start of fromDay .. end of toDay; null for invalid days. */
export function dayRangeToUnixBounds(fromDay: string, toDay: string): { fromUnix: number; toUnix: number } | null {
  const pattern = /^\d{4}-\d{2}-\d{2}$/;
  if (!pattern.test(fromDay) || !pattern.test(toDay)) return null;
  const fromDate = new Date(`${fromDay}T00:00:00Z`);
  const toDate = new Date(`${toDay}T23:59:59Z`);
  if (Number.isNaN(fromDate.getTime()) || Number.isNaN(toDate.getTime())) return null;
  if (fromDate.toISOString().slice(0, 10) !== fromDay || toDate.toISOString().slice(0, 10) !== toDay) return null;
  return { fromUnix: Math.floor(fromDate.getTime() / 1000), toUnix: Math.floor(toDate.getTime() / 1000) };
}

/** Keep setups whose signal time falls into the inclusive UTC day range. */
export function filterByDayRange(setups: AnalyticsSetup[], fromDay: string, toDay: string): AnalyticsSetup[] {
  const bounds = dayRangeToUnixBounds(fromDay, toDay);
  if (bounds === null) throw new Error(`invalid day range: ${fromDay}..${toDay}`);
  return setups.filter((setup) => setup.closeTimeUnix >= bounds.fromUnix && setup.closeTimeUnix <= bounds.toUnix);
}

/** Group sorted-by-time setups of one instrument into clusters with gaps <= maxGapSeconds. */
export function iterInstrumentClusters(sortedSetups: AnalyticsSetup[], maxGapSeconds: number = SETUP_CLUSTER_MAX_GAP_SECONDS): AnalyticsSetup[][] {
  const clusters: AnalyticsSetup[][] = [];
  let current: AnalyticsSetup[] = [];
  let previousTime: number | null = null;
  for (const setup of sortedSetups) {
    if (previousTime !== null && setup.closeTimeUnix - previousTime > maxGapSeconds) {
      if (current.length > 0) clusters.push(current);
      current = [];
    }
    current.push(setup);
    previousTime = setup.closeTimeUnix;
  }
  if (current.length > 0) clusters.push(current);
  return clusters;
}

export function iterSetupClusters(setups: AnalyticsSetup[], maxGapSeconds: number = SETUP_CLUSTER_MAX_GAP_SECONDS): AnalyticsSetup[][] {
  const byInstrument = new Map<string, AnalyticsSetup[]>();
  for (const setup of setups) {
    const key = setupInstrumentKey(setup);
    const list = byInstrument.get(key);
    if (list === undefined) byInstrument.set(key, [setup]);
    else list.push(setup);
  }
  const clusters: AnalyticsSetup[][] = [];
  for (const list of byInstrument.values()) {
    const sorted = [...list].sort((a, b) => a.closeTimeUnix - b.closeTimeUnix);
    clusters.push(...iterInstrumentClusters(sorted, maxGapSeconds));
  }
  return clusters;
}

function pickByMaturity(cluster: AnalyticsSetup[], maturity: number): { setup: AnalyticsSetup; ordinal: number; delaySeconds: number } {
  const first = cluster[0];
  if (first === undefined) throw new Error("maturity selection on empty cluster");
  const last = cluster[cluster.length - 1] ?? first;
  const start = first.closeTimeUnix;
  const end = last.closeTimeUnix;
  const target = start + maturity * (end - start);
  let best = first;
  let bestIndex = 0;
  let bestDistance = Math.abs(first.closeTimeUnix - target);
  for (let index = 1; index < cluster.length; index += 1) {
    const setup = cluster[index];
    if (setup === undefined) continue;
    const distance = Math.abs(setup.closeTimeUnix - target);
    // Strictly smaller distance wins; ties keep the earlier setup.
    if (distance < bestDistance) {
      best = setup;
      bestIndex = index;
      bestDistance = distance;
    }
  }
  return { setup: best, ordinal: bestIndex + 1, delaySeconds: best.closeTimeUnix - start };
}

/**
 * Select one setup per cluster closest to `start + maturity * (end - start)`;
 * on equal distance the earlier setup wins. `setups` must already be
 * filtered by month (if a month filter applies) — clustering sees the
 * filtered list only.
 */
export function selectByMaturity(setups: AnalyticsSetup[], maturity: number, maxGapSeconds: number = SETUP_CLUSTER_MAX_GAP_SECONDS): MaturitySelection {
  const selected: AnalyticsSetup[] = [];
  const ordinals: number[] = [];
  const delaysSeconds: number[] = [];
  let totalClustersCount = 0;
  for (const cluster of iterSetupClusters(setups, maxGapSeconds)) {
    totalClustersCount += 1;
    const picked = pickByMaturity(cluster, maturity);
    selected.push(picked.setup);
    ordinals.push(picked.ordinal);
    delaysSeconds.push(picked.delaySeconds);
  }
  const rows = [...selected].sort((a, b) => a.closeTimeUnix - b.closeTimeUnix);
  const delaysMean = mean(delaysSeconds);
  return {
    rows,
    avgSelectionOrdinal: mean(ordinals),
    avgSelectionDelayMinutes: delaysMean === null ? null : delaysMean / 60,
    totalClustersCount,
  };
}

export function setupClustersCount(setups: AnalyticsSetup[], maxGapSeconds: number = SETUP_CLUSTER_MAX_GAP_SECONDS): number {
  return iterSetupClusters(setups, maxGapSeconds).length;
}

/** Binary search: candle with exactly this open time, or undefined. */
export function findCandleByOpenTime(candles: OhlcCandle[], openTimeUnix: number): OhlcCandle | undefined {
  let low = 0;
  let high = candles.length - 1;
  while (low <= high) {
    const mid = Math.floor((low + high) / 2);
    const candle = candles[mid];
    if (candle === undefined) return undefined;
    if (candle.openTimeUnix === openTimeUnix) return candle;
    if (candle.openTimeUnix < openTimeUnix) low = mid + 1;
    else high = mid - 1;
  }
  return undefined;
}

/**
 * Entry price = close of the candle exactly `timeShiftMinutes` after the
 * signal candle (openTime === setupOpenTime + shift*60). Shift 0 returns
 * the signal candle close, i.e. the setup price.
 */
export function resolveEntry(
  candles: OhlcCandle[],
  setup: AnalyticsSetup,
  timeShiftMinutes: number,
): { entryPrice: number; entryCandle: OhlcCandle } | null {
  const candle = findCandleByOpenTime(candles, setup.openTimeUnix + timeShiftMinutes * MINUTE_SECONDS);
  if (candle === undefined) return null;
  return { entryPrice: candle.close, entryCandle: candle };
}

/**
 * Price-change gate. SHORT enters only when the shifted entry did not fall
 * below the setup close by more than the threshold; LONG is mirrored.
 * pct = 0 for short allows an equal or grown price.
 */
export function passesPriceChange(
  setupClosePrice: number,
  shiftedEntryPrice: number,
  direction: TradeDirection,
  acceptablePriceChangePct: number,
): boolean {
  if (direction === "short") {
    return shiftedEntryPrice >= setupClosePrice * (1 + acceptablePriceChangePct / 100);
  }
  return shiftedEntryPrice <= setupClosePrice * (1 - acceptablePriceChangePct / 100);
}

/** risk_return of one row: TP -> tp/sl, SL -> -1, otherwise null. */
export function riskReturn(status: LimitStatus, tpPct: number, slPct: number): number | null {
  if (status === "tp") return tpPct / slPct;
  if (status === "sl") return -1;
  return null;
}

/**
 * Walk minute candles from the close of the entry candle until the horizon
 * end and detect the first TP/SL touch. When one candle touches both
 * bounds, SL wins. Scan starts strictly after the entry candle: the entry
 * happens at its close, so its own high/low can no longer trigger levels.
 */
export function evaluateLimit(options: {
  candles: OhlcCandle[];
  setup: AnalyticsSetup;
  direction: TradeDirection;
  tpPct: number;
  slPct: number;
  timeShiftMinutes: number;
  horizonMinutes: number;
}): LimitEvaluation {
  const { candles, setup, direction, tpPct, slPct, timeShiftMinutes, horizonMinutes } = options;
  const noData: LimitEvaluation = {
    status: "no_data", entryPrice: null, entryTimeUnix: null,
    tpPrice: null, slPrice: null, closeTimeUnix: null, movePct: null, closeMinutes: null,
  };
  const entry = resolveEntry(candles, setup, timeShiftMinutes);
  if (entry === null) return noData;

  const entryPrice = entry.entryPrice;
  const entryTimeUnix = entry.entryCandle.openTimeUnix + MINUTE_SECONDS;
  const scanStartUnix = entryTimeUnix;
  const scanEndUnix = entryTimeUnix + horizonMinutes * MINUTE_SECONDS;
  const tpPrice = direction === "short" ? entryPrice * (1 - tpPct / 100) : entryPrice * (1 + tpPct / 100);
  const slPrice = direction === "short" ? entryPrice * (1 + slPct / 100) : entryPrice * (1 - slPct / 100);

  const touchesTp = (candle: OhlcCandle): boolean => (direction === "short" ? candle.low <= tpPrice : candle.high >= tpPrice);
  const touchesSl = (candle: OhlcCandle): boolean => (direction === "short" ? candle.high >= slPrice : candle.low <= slPrice);

  let start = lowerBound(candles, scanStartUnix);
  while (start < candles.length) {
    const candle = candles[start];
    if (candle === undefined || candle.openTimeUnix >= scanEndUnix) break;
    const hitSl = touchesSl(candle);
    if (hitSl || touchesTp(candle)) {
      const closeTimeUnix = candle.openTimeUnix;
      return {
        status: hitSl ? "sl" : "tp",
        entryPrice,
        entryTimeUnix,
        tpPrice: hitSl ? null : tpPrice,
        slPrice: hitSl ? slPrice : null,
        closeTimeUnix,
        movePct: hitSl ? -slPct : tpPct,
        closeMinutes: (closeTimeUnix - entryTimeUnix) / 60,
      };
    }
    start += 1;
  }
  return {
    status: "unclosed", entryPrice, entryTimeUnix,
    tpPrice: null, slPrice: null, closeTimeUnix: null, movePct: null, closeMinutes: null,
  };
}

/** Index of the first candle with openTime >= target (candles sorted ascending). */
export function lowerBound(candles: OhlcCandle[], targetUnix: number): number {
  let low = 0;
  let high = candles.length;
  while (low < high) {
    const mid = Math.floor((low + high) / 2);
    const candle = candles[mid];
    if (candle === undefined || candle.openTimeUnix < targetUnix) low = mid + 1;
    else high = mid;
  }
  return low;
}

/** Aggregate outcomes over closed TP/SL trades only (reference semantics). */
export function aggregateLimitOutcomes(evaluations: LimitEvaluation[], slPct: number): LimitAggregate {
  const tp = evaluations.filter((e) => e.status === "tp");
  const sl = evaluations.filter((e) => e.status === "sl");
  const unclosed = evaluations.filter((e) => e.status === "unclosed");
  const noData = evaluations.filter((e) => e.status === "no_data");
  const closed = [...tp, ...sl];
  const moves = closed.map((e) => e.movePct).filter((value): value is number => value !== null);
  const closeMinutes = closed.map((e) => e.closeMinutes).filter((value): value is number => value !== null);
  const avgMovePct = mean(moves);
  return {
    closedCount: closed.length,
    tpCount: tp.length,
    slCount: sl.length,
    unclosedCount: unclosed.length,
    noDataCount: noData.length,
    avgMovePct,
    medianMovePct: median(moves),
    winRatePct: closed.length > 0 ? (tp.length / closed.length) * 100 : null,
    avgCloseMinutes: mean(closeMinutes),
    medianCloseMinutes: median(closeMinutes),
    riskExpectancy: avgMovePct === null ? null : avgMovePct / slPct,
  };
}
