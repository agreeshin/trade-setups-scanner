import { apiFetch } from "./platform-api";
import type { ForecastRecipe, TrendRecipe } from "./forecast-recipe";
import type { ProbabilityForecast } from "./forecast-api";
import { sameForecastRecipe } from "./forecast-api";

export type TrendBendDays = "7" | "14" | "30" | "linear";
export type TrendMode = "mean" | "forecast" | "blend";
export interface TrendPoint { day: number; value: number; se: number | null }
export interface TrendSummary {
  method: "linear" | "p_spline";
  start: TrendPoint;
  end: TrendPoint;
  endpointSe: number | null;
  curve: TrendPoint[];
}
export interface TrendInput { day: number; value: number | null; weight: number }

export interface TrendProjectionRequest {
  forecast: TrendRecipe;
  meanR: number | null;
  trendMode: TrendMode;
  trendBlendLambda?: number;
}
export interface TrendProjection extends TrendProjectionRequest {
  forecastR: number | null;
  decisionR: number | null;
  curve: { day: number; value: number | null }[];
}
export interface TrendProjectionResponse { trend: TrendSummary | null; projection: TrendProjection }

const metric = (value: unknown): value is number | null => value === null || typeof value === "number" && Number.isFinite(value);
const finite = (value: unknown): value is number => typeof value === "number" && Number.isFinite(value);
const record = (value: unknown): value is Record<string, unknown> => value !== null && typeof value === "object" && !Array.isArray(value);
const curvePoint = (value: unknown): value is { day: number; value: number | null } => record(value) && finite(value.day) && metric(value.value);
function validProjectionCurve(value: unknown): value is TrendProjection["curve"] {
  if (!Array.isArray(value) || value.length > 3662) return false;
  const points: unknown[] = value;
  return points.every((point, index) => {
    const previous = points[index - 1];
    return curvePoint(point) && (index === 0 || curvePoint(previous) && point.day > previous.day);
  });
}
function validTrend(value: unknown): value is TrendSummary | null {
  if (value === null) return true;
  const point = (item: unknown): item is TrendPoint => record(item) && finite(item.day) && finite(item.value) && metric(item.se);
  return record(value) && (value.method === "linear" || value.method === "p_spline") && point(value.start) && point(value.end)
    && metric(value.endpointSe) && validProjectionCurve(value.curve) && value.curve.length > 0 && value.curve.every(point);
}

/** Same Rust solver as forecast matrices. Abort on changed data/unmount. */
export function fetchDailyTrend(points: TrendInput[], trendBendDays: TrendBendDays, signal: AbortSignal): Promise<TrendSummary | null>;
export function fetchDailyTrend(points: TrendInput[], trendBendDays: TrendBendDays, signal: AbortSignal, projection: TrendProjectionRequest): Promise<TrendProjectionResponse>;
export async function fetchDailyTrend(points: TrendInput[], trendBendDays: TrendBendDays, signal: AbortSignal, projection?: TrendProjectionRequest): Promise<TrendSummary | null | TrendProjectionResponse> {
  const response = await apiFetch("/v1/analytics/trend", {
    method: "POST", signal, headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ points, trendBendDays, ...(projection ? { projection } : {}) }),
  });
  if (!response.ok) throw new Error(`HTTP ${response.status}: ${(await response.text()).slice(0, 120)}`);
  const body: unknown = await response.json();
  if (!record(body) || !validTrend(body.trend)) throw new Error("Сервер вернул несовместимую кривую тренда.");
  if (!projection) return body.trend;
  const actual = body.projection;
  if (!record(actual) || !sameForecastRecipe(actual.forecast, projection.forecast) || actual.meanR !== projection.meanR
    || actual.trendMode !== projection.trendMode || actual.trendBlendLambda !== projection.trendBlendLambda
    || !metric(actual.forecastR) || !metric(actual.decisionR) || !validProjectionCurve(actual.curve)
    || actual.forecastR !== (body.trend?.end.value ?? null)
    || (actual.decisionR !== null && (actual.curve.length === 0 ? points.length > 0 : actual.curve.at(-1)?.value !== actual.decisionR))) {
    throw new Error("Сервер не подтвердил рецепт, M, режим, λ или endpoint кривой решения.");
  }
  return body as unknown as TrendProjectionResponse;
}

export interface AnalyticsSet {
  setId: string;
  kind: "batch" | "run";
  numericId: number;
  label: string;
  algorithmId: string;
  algorithmVersion: string | null;
  params: Record<string, string | number>;
  exchange: string;
  category: string;
  symbol: string | null;
  setupCount: number;
  fromUnix: number | null;
  toUnix: number | null;
  createdAt: string;
}

export interface AnalyticsSetupRow {
  exchange: string;
  category: string;
  symbol: string;
  openTimeUnix: number;
  closeTimeUnix: number;
  closePrice: string;
}

export interface SelectionMetrics {
  totalMatchingSetups: number;
  avgSelectionOrdinal: number | null;
  avgSelectionDelayMinutes: number | null;
  totalClustersCount: number | null;
  cachedSelection: boolean;
}

export interface MatrixCellPayload {
  meanR?: number | null;
  forecastR?: number | null;
  decisionR?: number | null;
  forecast?: ForecastRecipe;
  observationCutoff?: number;
  forecastEstimate?: ProbabilityForecast;
  endpointSe?: number | null;
  meanRiskExpectancy?: number | null;
  tpPct: number;
  slPct: number;
  direction: "short" | "long";
  month: string | null;
  setupMaturity: number | null;
  timeShiftMinutes: number;
  horizonMinutes: number;
  priceChangePct: number | null;
  totalMatchingSetups: number;
  filteredSetupsCount: number;
  eligibleSetupsCount: number;
  priceChangeFilteredCount: number;
  avgSelectionOrdinal: number | null;
  avgSelectionDelayMinutes: number | null;
  totalClustersCount: number | null;
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
  riskExpectancy: number | null;
}

export interface TradeRow {
  setup: AnalyticsSetupRow;
  direction: "short" | "long";
  tpPct: number;
  slPct: number;
  status: string;
  entryPrice: number | null;
  tpPrice: number | null;
  slPrice: number | null;
  closeTimeUnix: number | null;
  riskReturn: number | null;
}

export interface MatrixCellRef {
  meanR?: number | null;
  forecastR?: number | null;
  decisionR?: number | null;
  forecast?: ForecastRecipe;
  observationCutoff?: number;
  forecastEstimate?: ProbabilityForecast;
  endpointSe?: number | null;
  meanRiskExpectancy?: number | null;
  tpPct: number;
  slPct: number;
  avgMovePct: number | null;
  riskExpectancy: number | null;
}

export interface AnalyticsQueryParams {
  forecast?: ForecastRecipe;
  observationCutoff?: number;
  trendBlendLambda?: number;
  trendBendDays?: TrendBendDays;
  trendMode?: TrendMode;
  set: string;
  direction?: "short" | "long";
  month?: string;
  setupMaturity?: number;
  clusterGapHours?: number;
  fromDay?: string;
  toDay?: string;
  tpPct?: number;
  slPct?: number;
  timeShiftMinutes?: number;
  horizonMinutes?: number;
  priceChangePct?: number;
}

function toQuery(params: AnalyticsQueryParams): URLSearchParams {
  const search = new URLSearchParams({ set: params.set });
  if (params.forecast !== undefined) search.set("forecast", JSON.stringify(params.forecast));
  if (params.observationCutoff !== undefined) search.set("observationCutoff", String(params.observationCutoff));
  if (params.trendBlendLambda !== undefined) search.set("trendBlendLambda", String(params.trendBlendLambda));
  if (params.trendBendDays !== undefined) search.set("trendBendDays", params.trendBendDays);
  if (params.trendMode !== undefined) search.set("trendMode", params.trendMode);
  if (params.direction !== undefined) search.set("direction", params.direction);
  if (params.month !== undefined) search.set("month", params.month);
  if (params.setupMaturity !== undefined) search.set("setupMaturity", String(params.setupMaturity));
  if (params.clusterGapHours !== undefined) search.set("clusterGapHours", String(params.clusterGapHours));
  if (params.month !== undefined) search.set("month", params.month);
  if (params.fromDay !== undefined) search.set("fromDay", params.fromDay);
  if (params.toDay !== undefined) search.set("toDay", params.toDay);
  if (params.tpPct !== undefined) search.set("tpPct", String(params.tpPct));
  if (params.slPct !== undefined) search.set("slPct", String(params.slPct));
  if (params.timeShiftMinutes !== undefined) search.set("timeShiftMinutes", String(params.timeShiftMinutes));
  if (params.horizonMinutes !== undefined) search.set("horizonMinutes", String(params.horizonMinutes));
  if (params.priceChangePct !== undefined) search.set("priceChangePct", String(params.priceChangePct));
  return search;
}

async function getJson<T>(path: string, signal?: AbortSignal): Promise<T> {
  const response = await apiFetch(path, signal ? { signal } : undefined);
  if (!response.ok) {
    throw new Error(`HTTP ${response.status}: ${(await response.text()).slice(0, 120)}`);
  }
  return (await response.json()) as T;
}

export async function fetchAnalyticsSets(): Promise<AnalyticsSet[]> {
  const body = await getJson<{ sets: AnalyticsSet[] }>("/v1/analytics/sets");
  return body.sets;
}

export async function fetchAnalyticsMonths(set: string): Promise<string[]> {
  const body = await getJson<{ months: string[] }>(`/v1/analytics/months?${toQuery({ set })}`);
  return body.months;
}

export async function fetchAnalyticsSetups(params: AnalyticsQueryParams, signal?: AbortSignal): Promise<{ setups: AnalyticsSetupRow[] } & SelectionMetrics> {
  return getJson(`/v1/analytics/setups?${toQuery(params)}`, signal);
}

export async function fetchMatrixCells(params: AnalyticsQueryParams): Promise<{ items: MatrixCellRef[] } & SelectionMetrics> {
  return getJson(`/v1/analytics/matrix-cells?${toQuery(params)}`);
}

export async function fetchMatrixCell(params: AnalyticsQueryParams): Promise<MatrixCellPayload & { cached: boolean; cachedSelection: boolean }> {
  return getJson(`/v1/analytics/matrix-cell?${toQuery(params)}`);
}

export interface MatrixGridItem extends MatrixCellRef {
  cached: boolean;
}

export async function fetchMatrixGrid(params: AnalyticsQueryParams, tpPcts: number[], slPcts: number[]): Promise<{ items: MatrixGridItem[] } & SelectionMetrics> {
  const search = toQuery(params);
  search.set("tpPct", tpPcts.join(","));
  search.set("slPct", slPcts.join(","));
  const result = await getJson<{ items: MatrixGridItem[] } & SelectionMetrics>(`/v1/analytics/matrix-grid?${search.toString()}`);
  if (params.forecast && params.trendMode !== "mean" && !result.items.every((cell) => sameForecastRecipe(cell.forecast, params.forecast!) && cell.observationCutoff === params.observationCutoff
    && cell.meanR !== undefined && cell.forecastR !== undefined && cell.decisionR !== undefined)) throw new Error("Сервер не подтвердил рецепт, метрики или cutoff рассчитанной матрицы.");
  return result;
}

export async function fetchTradeRows(params: AnalyticsQueryParams, signal?: AbortSignal): Promise<{ items: TradeRow[]; cachedCount: number }> {
  return getJson(`/v1/analytics/trades?${toQuery(params)}`, signal);
}

export async function fetchTradeRow(setup: AnalyticsSetupRow, params: AnalyticsQueryParams, signal?: AbortSignal): Promise<TradeRow & { cached: boolean }> {
  return getJson(
    `/v1/analytics/trades/${encodeURIComponent(setup.symbol)}/${setup.closeTimeUnix}?${toQuery(params)}`, signal,
  );
}

export async function fetchAnalyticsCacheSummary(): Promise<{ tables: { name: string; count: number }[]; total: number }> {
  return getJson("/v1/analytics/cache/summary");
}

export async function clearAnalyticsCache(): Promise<{ deleted: { name: string; count: number }[]; total: number }> {
  const response = await apiFetch("/v1/analytics/cache", { method: "DELETE" });
  if (!response.ok) {
    throw new Error(`HTTP ${response.status}`);
  }
  return (await response.json()) as { deleted: { name: string; count: number }[]; total: number };
}
