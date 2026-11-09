import { apiFetch } from "./platform-api";

export interface AnalyticsSet {
  setId: string;
  kind: "batch" | "run";
  numericId: number;
  label: string;
  algorithmId: string;
  algorithmVersion: string | null;
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
  tpPct: number;
  slPct: number;
  avgMovePct: number | null;
  riskExpectancy: number | null;
}

export interface AnalyticsQueryParams {
  set: string;
  direction?: "short" | "long";
  month?: string;
  setupMaturity?: number;
  tpPct?: number;
  slPct?: number;
  timeShiftMinutes?: number;
  horizonMinutes?: number;
  priceChangePct?: number;
}

function toQuery(params: AnalyticsQueryParams): URLSearchParams {
  const search = new URLSearchParams({ set: params.set });
  if (params.direction !== undefined) search.set("direction", params.direction);
  if (params.month !== undefined) search.set("month", params.month);
  if (params.setupMaturity !== undefined) search.set("setupMaturity", String(params.setupMaturity));
  if (params.tpPct !== undefined) search.set("tpPct", String(params.tpPct));
  if (params.slPct !== undefined) search.set("slPct", String(params.slPct));
  if (params.timeShiftMinutes !== undefined) search.set("timeShiftMinutes", String(params.timeShiftMinutes));
  if (params.horizonMinutes !== undefined) search.set("horizonMinutes", String(params.horizonMinutes));
  if (params.priceChangePct !== undefined) search.set("priceChangePct", String(params.priceChangePct));
  return search;
}

async function getJson<T>(path: string): Promise<T> {
  const response = await apiFetch(path);
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

export async function fetchAnalyticsSetups(params: AnalyticsQueryParams): Promise<{ setups: AnalyticsSetupRow[] } & SelectionMetrics> {
  return getJson(`/v1/analytics/setups?${toQuery(params)}`);
}

export async function fetchMatrixCells(params: AnalyticsQueryParams): Promise<{ items: MatrixCellRef[] } & SelectionMetrics> {
  return getJson(`/v1/analytics/matrix-cells?${toQuery(params)}`);
}

export async function fetchMatrixCell(params: AnalyticsQueryParams): Promise<MatrixCellPayload & { cached: boolean; cachedSelection: boolean }> {
  return getJson(`/v1/analytics/matrix-cell?${toQuery(params)}`);
}

export async function fetchTradeRows(params: AnalyticsQueryParams): Promise<{ items: TradeRow[]; cachedCount: number }> {
  return getJson(`/v1/analytics/trades?${toQuery(params)}`);
}

export async function fetchTradeRow(setup: AnalyticsSetupRow, params: AnalyticsQueryParams): Promise<TradeRow & { cached: boolean }> {
  return getJson(
    `/v1/analytics/trades/${encodeURIComponent(setup.symbol)}/${setup.closeTimeUnix}?${toQuery(params)}`,
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
