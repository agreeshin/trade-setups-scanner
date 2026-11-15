import type { TradeRow } from "./analytics-api";

const SECONDS_PER_DAY = 86400;
export interface DailyExpectancyPoint {
  day: string;
  dayStartUnix: number;
  riskExpectancy: number | null;
  closedCount: number;
  tpCount: number;
  slCount: number;
  computedCount: number;
  cachedCount: number;
}
export interface DailyExpectancyCoordinate extends DailyExpectancyPoint { x: number }
export interface DailyExpectancyTrend {
  slopePerDay: number;
  meanX: number;
  meanY: number;
  start: { x: number; riskExpectancy: number };
  end: { x: number; riskExpectancy: number };
  totalChange: number;
}
export interface DailyExpectancyModel {
  originUnix: number;
  points: DailyExpectancyCoordinate[];
  segments: DailyExpectancyCoordinate[][];
  trend: DailyExpectancyTrend | null;
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
    closedCount: bucket.closedCount, tpCount: bucket.tpCount, slCount: bucket.slCount,
    computedCount: bucket.computedCount, cachedCount: bucket.cachedCount,
  }));
}

export function utcDayStart(unix: number): number {
  if (!Number.isFinite(unix) || Math.abs(unix * 1000) > 8.64e15) throw new RangeError("Invalid expectancy signal timestamp");
  return Math.floor(unix / SECONDS_PER_DAY) * SECONDS_PER_DAY;
}

/** Historical model only. The caller fixes the displayed period's origin; no live forecasting. */
export function buildDailyExpectancyModel(items: readonly DailyExpectancyPoint[], periodStartUnix: number): DailyExpectancyModel {
  const originUnix = utcDayStart(periodStartUnix);
  const points = items.map((item) => ({ ...item, x: (item.dayStartUnix - originUnix) / SECONDS_PER_DAY }));
  const segments: DailyExpectancyCoordinate[][] = [];
  const finite: { x: number; value: number }[] = [];
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
      finite.push({ x: point.x, value: point.riskExpectancy });
    }
  }
  if (segment.length > 0) segments.push(segment);
  let trend: DailyExpectancyTrend | null = null;
  if (finite.length > 1) {
    const meanX = finite.reduce((sum, point) => sum + point.x, 0) / finite.length;
    const meanY = finite.reduce((sum, point) => sum + point.value, 0) / finite.length;
    let covariance = 0;
    let variance = 0;
    for (const point of finite) {
      covariance += (point.x - meanX) * (point.value - meanY);
      variance += (point.x - meanX) ** 2;
    }
    if (variance > 0) {
      const slopePerDay = covariance / variance;
      const fittedAt = (x: number) => meanY + slopePerDay * (x - meanX);
      const first = finite[0]!;
      const last = finite[finite.length - 1]!;
      const start = { x: first.x, riskExpectancy: fittedAt(first.x) };
      const end = { x: last.x, riskExpectancy: fittedAt(last.x) };
      trend = { slopePerDay, meanX, meanY, start, end, totalChange: end.riskExpectancy - start.riskExpectancy };
    }
  }
  return { originUnix, points, segments, trend };
}
