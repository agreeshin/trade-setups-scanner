import { apiFetch } from "./platform-api";

export interface SetupsAnalyticsStudyMeta {
  studyId: number;
  externalName: string;
  setupCount: number;
  generationId: number | null;
  artifactId: number | null;
}

export interface SetupsAnalyticsTotals {
  selected: number;
  gatePassed: number;
  closed: number;
  meanR: number | null;
}

export interface SetupsAnalyticsStudy extends SetupsAnalyticsTotals {
  studyId: number;
}

export type SetupsAnalyticsStatus = "tp" | "sl" | "unclosed" | "no_data" | "gate_failed";

export interface SetupsAnalyticsSetup {
  id: number;
  symbol: string;
  exchange: string;
  slotId: number;
  studyId: number | null;
  direction: "short" | "long";
  setupTimeUnix: number;
  entryTimeUnix: number | null;
  entryPrice: number | null;
  gateThreshold: number | null;
  tpPrice: number | null;
  slPrice: number | null;
  closeTimeUnix: number | null;
  riskReturn: number | null;
  status: SetupsAnalyticsStatus;
}

export interface SetupsAnalyticsResult {
  totals: SetupsAnalyticsTotals;
  studies: SetupsAnalyticsStudy[];
  setups: SetupsAnalyticsSetup[];
}

export class SetupsAnalyticsApiError extends Error {
  constructor(message: string, readonly status?: number) {
    super(message);
  }
}

const isRecord = (value: unknown): value is Record<string, unknown> =>
  value !== null && typeof value === "object" && !Array.isArray(value);
const isId = (value: unknown): value is number =>
  typeof value === "number" && Number.isSafeInteger(value) && value > 0;
const isCount = (value: unknown): value is number =>
  typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
const isFiniteNumber = (value: unknown): value is number =>
  typeof value === "number" && Number.isFinite(value);
const isUnix = (value: unknown): value is number =>
  typeof value === "number" && Number.isSafeInteger(value);
const isDirection = (value: unknown): value is "short" | "long" =>
  value === "short" || value === "long";
const isStatus = (value: unknown): value is SetupsAnalyticsStatus =>
  value === "tp" || value === "sl" || value === "unclosed" || value === "no_data" || value === "gate_failed";

function hasAggregateFields(item: Record<string, unknown>): boolean {
  return isCount(item.selected) && isCount(item.gatePassed) && isCount(item.closed)
    && (item.meanR === null || isFiniteNumber(item.meanR));
}

function isStudyMeta(value: unknown): value is SetupsAnalyticsStudyMeta {
  if (!isRecord(value)) return false;
  return isId(value.studyId)
    && typeof value.externalName === "string"
    && isCount(value.setupCount)
    && (value.generationId === null || isId(value.generationId))
    && (value.artifactId === null || isId(value.artifactId));
}

function isTotals(value: unknown): value is SetupsAnalyticsTotals {
  return isRecord(value) && hasAggregateFields(value);
}

function isStudy(value: unknown): value is SetupsAnalyticsStudy {
  return isRecord(value) && isId(value.studyId) && hasAggregateFields(value);
}

function isSetup(value: unknown): value is SetupsAnalyticsSetup {
  if (!isRecord(value)) return false;
  return isId(value.id)
    && typeof value.symbol === "string"
    && typeof value.exchange === "string"
    && isId(value.slotId)
    && (value.studyId === null || isId(value.studyId))
    && isDirection(value.direction)
    && isUnix(value.setupTimeUnix)
    && (value.entryTimeUnix === null || isUnix(value.entryTimeUnix))
    && (value.entryPrice === null || isFiniteNumber(value.entryPrice))
    && (value.gateThreshold === null || isFiniteNumber(value.gateThreshold))
    && (value.tpPrice === null || isFiniteNumber(value.tpPrice))
    && (value.slPrice === null || isFiniteNumber(value.slPrice))
    && (value.closeTimeUnix === null || isUnix(value.closeTimeUnix))
    && (value.riskReturn === null || isFiniteNumber(value.riskReturn))
    && isStatus(value.status);
}

function expectArray<T>(value: unknown, check: (item: unknown) => item is T, label: string): T[] {
  if (!Array.isArray(value)) throw new SetupsAnalyticsApiError(`Некорректный ответ расчёта: поле ${label}`);
  if (!value.every(check)) throw new SetupsAnalyticsApiError(`Некорректный ответ расчёта: поле ${label}`);
  return value;
}

async function readBody(response: Response): Promise<unknown> {
  const body: unknown = await response.json().catch(() => null);
  if (!response.ok) {
    const code = isRecord(body) && typeof body.error === "string" ? body.error : "";
    if (code === "setups_analytics_limit_exceeded") {
      throw new SetupsAnalyticsApiError("Сузьте диапазон: в выборке больше 5000 сетапов.", response.status);
    }
    throw new SetupsAnalyticsApiError(`Расчёт недоступен: HTTP ${response.status}`, response.status);
  }
  return body;
}

export async function fetchSetupsAnalyticsStudies(
  algorithmId: string,
  fromUnix: number,
  toUnix: number,
  signal?: AbortSignal,
): Promise<SetupsAnalyticsStudyMeta[]> {
  const params = new URLSearchParams({ algorithmId, fromUnix: String(fromUnix), toUnix: String(toUnix) });
  const response = await apiFetch(`/v1/setups/analytics/studies?${params}`, signal === undefined ? {} : { signal });
  const body = await readBody(response);
  if (!isRecord(body) || !Array.isArray(body.studies)) {
    throw new SetupsAnalyticsApiError("Некорректный ответ расчёта: список исследований");
  }
  return expectArray(body.studies, isStudyMeta, "studies");
}

export async function fetchSetupsAnalytics(
  algorithmId: string,
  fromUnix: number,
  toUnix: number,
  studies: readonly number[] | null,
  signal?: AbortSignal,
): Promise<SetupsAnalyticsResult> {
  const params = new URLSearchParams({ algorithmId, fromUnix: String(fromUnix), toUnix: String(toUnix) });
  if (studies !== null) params.set("studies", studies.join(","));
  const response = await apiFetch(`/v1/setups/analytics?${params}`, signal === undefined ? {} : { signal });
  const body = await readBody(response);
  if (!isRecord(body) || !isTotals(body.totals) || !Array.isArray(body.studies) || !Array.isArray(body.setups)) {
    throw new SetupsAnalyticsApiError("Некорректный ответ расчёта");
  }
  return {
    totals: body.totals,
    studies: expectArray(body.studies, isStudy, "studies"),
    setups: expectArray(body.setups, isSetup, "setups"),
  };
}

export async function fetchExpectancyPngUrl(artifactId: number, signal?: AbortSignal): Promise<string> {
  const response = await apiFetch(`/v1/setups/analytics/expectancy-png/${artifactId}`, signal === undefined ? {} : { signal });
  if (!response.ok) {
    throw new SetupsAnalyticsApiError(`PNG исследования недоступен: HTTP ${response.status}`, response.status);
  }
  return URL.createObjectURL(await response.blob());
}

export interface StudyExpectancyPoint {
  dayUnix: number;
  meanR: number | null;
  tpCount: number;
  slCount: number;
}

export interface StudyTrendPoint {
  dayUnix: number;
  value: number;
}

export interface StudyExpectancySeries {
  studyId: number;
  observationCutoff: number;
  meanR: number;
  daily: StudyExpectancyPoint[];
  trend: { method: string | null; points: StudyTrendPoint[] };
}

function isStudyExpectancyPoint(value: unknown): value is StudyExpectancyPoint {
  return isRecord(value)
    && isUnix(value.dayUnix)
    && (value.meanR === null || isFiniteNumber(value.meanR))
    && Number.isInteger(value.tpCount)
    && Number.isInteger(value.slCount);
}

function isStudyTrendPoint(value: unknown): value is StudyTrendPoint {
  return isRecord(value) && isUnix(value.dayUnix) && isFiniteNumber(value.value);
}

export interface StudyLivePoint {
  dayUnix: number;
  meanR: number;
  closedCount: number;
}

export async function fetchStudyExpectancy(studyId: number, live: readonly StudyLivePoint[], signal?: AbortSignal): Promise<StudyExpectancySeries> {
  const response = await apiFetch(`/v1/setups/analytics/study-expectancy/${studyId}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ live }),
    ...(signal === undefined ? {} : { signal }),
  });
  const body: unknown = await readBody(response);
  if (!isRecord(body)
    || typeof body.studyId !== "number"
    || !Number.isInteger(body.studyId)
    || !isUnix(body.observationCutoff)
    || !isFiniteNumber(body.meanR)) {
    throw new SetupsAnalyticsApiError("Некорректный ответ: серия матожидания исследования");
  }
  const trend = isRecord(body.trend) ? body.trend : null;
  return {
    studyId: body.studyId,
    observationCutoff: body.observationCutoff,
    meanR: body.meanR,
    daily: Array.isArray(body.daily) ? expectArray(body.daily, isStudyExpectancyPoint, "daily") : [],
    trend: {
      method: trend !== null && typeof trend.method === "string" ? trend.method : null,
      points: trend !== null && Array.isArray(trend.points) ? expectArray(trend.points, isStudyTrendPoint, "trend.points") : [],
    },
  };
}
