import type { TradeRow } from "./analytics-api";

const SECONDS_PER_DAY = 86400;
export interface DailyExpectancyPoint {
  day: string;
  dayStartUnix: number;
  riskExpectancy: number | null;
  /** Exact sum from the calculated closed rows; optional for older chart consumers. */
  sumR?: number | null;
  closedCount: number;
  tpCount: number;
  slCount: number;
  computedCount: number;
  cachedCount: number;
}
export interface DailyExpectancyCoordinate extends DailyExpectancyPoint { x: number }
export interface DailyExpectancyModel {
  originUnix: number;
  points: DailyExpectancyCoordinate[];
  segments: DailyExpectancyCoordinate[][];
}

/** One UTC day per represented signal date; no missing-day zero fill or browser timezone. */
export function buildDailyExpectancy(rows: readonly (TradeRow & { fromCache: boolean })[]): DailyExpectancyPoint[] {
  const buckets = new Map<number, { sum: number; closedCount: number; tpCount: number; slCount: number; computedCount: number; cachedCount: number }>();
  for (const row of rows) {
    const dayStartUnix = utcDayStart(row.setup.closeTimeUnix);
    const bucket = buckets.get(dayStartUnix) ?? { sum: 0, closedCount: 0, tpCount: 0, slCount: 0, computedCount: 0, cachedCount: 0 };
    bucket.computedCount += 1;
    if (row.fromCache) bucket.cachedCount += 1;
    if ((row.status === "tp" || row.status === "sl") && row.riskReturn !== null && Number.isFinite(row.riskReturn)) {
      bucket.sum += row.riskReturn;
      bucket.closedCount += 1;
      if (row.status === "tp") bucket.tpCount += 1;
      else bucket.slCount += 1;
    }
    buckets.set(dayStartUnix, bucket);
  }
  return [...buckets.entries()].sort(([left], [right]) => left - right).map(([dayStartUnix, bucket]) => ({
    day: new Date(dayStartUnix * 1000).toISOString().slice(0, 10), dayStartUnix,
    riskExpectancy: bucket.closedCount > 0 ? bucket.sum / bucket.closedCount : null,
    sumR: bucket.closedCount > 0 ? bucket.sum : null,
    closedCount: bucket.closedCount, tpCount: bucket.tpCount, slCount: bucket.slCount,
    computedCount: bucket.computedCount, cachedCount: bucket.cachedCount,
  }));
}

export type ObservedDailyMode = "mean" | "sum";
/** Presentation only: never pass this projection back as model training data. */
export function observedDailyPoints(items: readonly DailyExpectancyPoint[], mode: ObservedDailyMode): DailyExpectancyPoint[] {
  return items.map((point) => ({ ...point, riskExpectancy: mode === "mean" ? point.riskExpectancy
    : point.sumR !== undefined ? point.sumR : point.riskExpectancy === null ? null : point.riskExpectancy * point.closedCount }));
}

export function utcDayStart(unix: number): number {
  if (!Number.isFinite(unix) || Math.abs(unix * 1000) > 8.64e15) throw new RangeError("Invalid expectancy signal timestamp");
  return Math.floor(unix / SECONDS_PER_DAY) * SECONDS_PER_DAY;
}

/** Calendar geometry only. Fitting/SE belong to the shared Rust analytics solver. */
export function buildDailyExpectancyModel(items: readonly DailyExpectancyPoint[], periodStartUnix: number): DailyExpectancyModel {
  const originUnix = utcDayStart(periodStartUnix);
  const points = items.map((item) => ({ ...item, x: (item.dayStartUnix - originUnix) / SECONDS_PER_DAY }));
  const segments: DailyExpectancyCoordinate[][] = [];
  let segment: DailyExpectancyCoordinate[] = [];
  let previousDay = -Infinity;
  for (const point of points) {
    if (utcDayStart(point.dayStartUnix) !== point.dayStartUnix || point.dayStartUnix <= previousDay) throw new RangeError("Expectancy days must be unique ascending UTC midnights");
    previousDay = point.dayStartUnix;
    if (point.riskExpectancy === null || !Number.isFinite(point.riskExpectancy)) {
      if (segment.length > 0) segments.push(segment);
      segment = [];
    } else {
      segment.push(point);
    }
  }
  if (segment.length > 0) segments.push(segment);
  return { originUnix, points, segments };
}
