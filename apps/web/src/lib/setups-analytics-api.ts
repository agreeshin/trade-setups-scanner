import { apiFetch } from "./platform-api";
import { parseSetupTradeOutcome } from "./setup-outcomes-api";
import type { SetupRow } from "../routes/platform-controls";
import { buildForecastRecipe, isTrendBendDays, restoreForecastRecipeDraft, type ForecastRecipe } from "./forecast-recipe";
import type { TrendBendDays } from "./analytics-api";

export interface SetupsAnalyticsStudyMeta {
  studyId: number;
  externalName: string;
  setupCount: number;
  generationId: number | null;
  artifactId: number | null;
  /** Additive, saved scalar projections. Missing means an older API, not zero. */
  status?: string;
  bestScore?: number | null;
  forecastBest?: { forecastR: number; meanR: number; tpPct: number; slPct: number } | null;
  bestCell?: { tpPct: number; slPct: number; rawR: number } | null;
  metricsUnavailableReason?: string | null;
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
  closePrice?: number | null;
  riskReturn: number | null;
  status: SetupsAnalyticsStatus;
}

export interface SetupsAnalyticsResult {
  totals: SetupsAnalyticsTotals;
  studies: SetupsAnalyticsStudy[];
  setups: SetupsAnalyticsSetup[];
  /** Fingerprint covers the full calculated cohort and its frozen saved outcomes. */
  snapshot?: SetupsAnalyticsSnapshot;
}
export interface SetupsAnalyticsSnapshot {
  snapshotId: number;
  observationCutoff: number;
  fingerprint: string;
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
    && (value.artifactId === null || isId(value.artifactId))
    && (value.status === undefined || typeof value.status === "string")
    && (value.bestScore === undefined || value.bestScore === null || isFiniteNumber(value.bestScore))
    && (value.forecastBest === undefined || value.forecastBest === null || (isRecord(value.forecastBest)
      && [value.forecastBest.forecastR, value.forecastBest.meanR, value.forecastBest.tpPct, value.forecastBest.slPct].every(isFiniteNumber)))
    && (value.bestCell === undefined || value.bestCell === null || (isRecord(value.bestCell)
      && [value.bestCell.tpPct, value.bestCell.slPct, value.bestCell.rawR].every(isFiniteNumber)))
    && (value.metricsUnavailableReason === undefined || value.metricsUnavailableReason === null || typeof value.metricsUnavailableReason === "string");
}

export function isSetupsAnalyticsSnapshot(value: unknown): value is SetupsAnalyticsSnapshot {
  return isRecord(value) && isCount(value.snapshotId) && isCount(value.observationCutoff)
    && value.observationCutoff % 60 === 0 && typeof value.fingerprint === "string" && /^[a-f0-9]{64}$/i.test(value.fingerprint);
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
    && (value.closePrice === undefined || value.closePrice === null || isFiniteNumber(value.closePrice))
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
    if (code === "setups_analytics_market_data_unavailable") {
      throw new SetupsAnalyticsApiError("Не удалось прочитать свечи из сервиса данных. Повторите расчёт после восстановления сервиса; это не отсутствие сделок.", response.status);
    }
    if (code === "setups_analytics_studies_limit_exceeded") {
      throw new SetupsAnalyticsApiError("Слишком много исследований для одного списка. Сузьте диапазон или поиск; история сетапов остаётся доступной.", response.status);
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
  q = "",
): Promise<SetupsAnalyticsStudyMeta[]> {
  const params = new URLSearchParams({ algorithmId, fromUnix: String(fromUnix), toUnix: String(toUnix) });
  if (q !== "") params.set("q", q);
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
  q = "",
): Promise<SetupsAnalyticsResult> {
  const params = new URLSearchParams({ algorithmId, fromUnix: String(fromUnix), toUnix: String(toUnix) });
  if (q !== "") params.set("q", q);
  if (studies !== null) params.set("studies", studies.join(","));
  const response = await apiFetch(`/v1/setups/analytics?${params}`, { method: "POST", ...(signal === undefined ? {} : { signal }) });
  const body = await readBody(response);
  if (!isRecord(body) || !isTotals(body.totals) || !Array.isArray(body.studies) || !Array.isArray(body.setups)
    || (body.snapshot !== undefined && !isSetupsAnalyticsSnapshot(body.snapshot))) {
    throw new SetupsAnalyticsApiError("Некорректный ответ расчёта");
  }
  return {
    totals: body.totals,
    studies: expectArray(body.studies, isStudy, "studies"),
    setups: expectArray(body.setups, isSetup, "setups"),
    ...(body.snapshot === undefined ? {} : { snapshot: body.snapshot }),
  };
}

export interface SetupHistoryFilter {
  algorithmId: string;
  fromUnix: number;
  toUnix: number;
  studies: readonly number[] | null;
  q: string;
  hideGateFailed: boolean;
}
export interface SetupHistoryPage { setups: SetupRow[]; total: number; offset: number; limit: number; snapshotId: number }
export interface TelegramTextEntity { type: "bold" | "code"; offset: number; length: number }
export interface SetupDetailsResponse {
  setup: SetupRow;
  telegramMessage: { text: string; entities: TelegramTextEntity[] } | null;
  reason?: string | null;
}

function savedSetup(value: unknown): SetupRow {
  if (!isRecord(value) || !isId(value.id) || typeof value.symbol !== "string" || typeof value.exchange !== "string"
    || typeof value.algorithmId !== "string" || typeof value.closePrice !== "string" || !isUnix(value.openTimeUnix) || !isUnix(value.closeTimeUnix)) {
    throw new SetupsAnalyticsApiError("Некорректный ответ: сохранённый сетап");
  }
  return { ...value, tradeOutcome: parseSetupTradeOutcome(value.tradeOutcome) } as unknown as SetupRow;
}

export async function fetchSetupHistory(filter: SetupHistoryFilter, offset: number, snapshotId: number | undefined, signal: AbortSignal): Promise<SetupHistoryPage> {
  const params = new URLSearchParams({ algorithmId: filter.algorithmId, fromUnix: String(filter.fromUnix), toUnix: String(filter.toUnix),
    q: filter.q, hideGateFailed: String(filter.hideGateFailed), offset: String(offset), limit: "25" });
  if (filter.studies !== null) params.set("studies", filter.studies.join(","));
  if (snapshotId !== undefined) params.set("snapshotId", String(snapshotId));
  const response = await apiFetch(`/v1/setups?${params}`, { signal });
  const body = await readBody(response);
  if (!isRecord(body) || !Array.isArray(body.setups) || body.setups.length > 25 || !isCount(body.total) || body.offset !== offset
    || body.limit !== 25 || !isCount(body.snapshotId) || (snapshotId !== undefined && body.snapshotId !== snapshotId)) throw new SetupsAnalyticsApiError("Некорректный ответ: история сетапов");
  return { setups: body.setups.map(savedSetup), total: body.total, offset: body.offset, limit: body.limit, snapshotId: body.snapshotId };
}

export async function fetchSetupDetails(id: number, signal: AbortSignal): Promise<SetupDetailsResponse> {
  const response = await apiFetch(`/v1/setups/${id}/details`, { signal });
  const body = await readBody(response);
  if (!isRecord(body)) throw new SetupsAnalyticsApiError("Некорректный ответ: подробности сетапа");
  const setup = savedSetup(body.setup);
  if (setup.id !== id) throw new SetupsAnalyticsApiError("Изменилась идентичность сетапа");
  if (body.telegramMessage === null) return { setup, telegramMessage: null, reason: typeof body.reason === "string" ? body.reason : null };
  const message = body.telegramMessage;
  if (!isRecord(message) || typeof message.text !== "string" || message.text.length > 4096 || !Array.isArray(message.entities) || message.entities.length > 32) {
    throw new SetupsAnalyticsApiError("Некорректный канонический текст сетапа");
  }
  let end = 0;
  const entities: TelegramTextEntity[] = [];
  const text = message.text;
  const boundary = (at: number) => !(at > 0 && at < text.length && /[\uDC00-\uDFFF]/.test(text[at]!) && /[\uD800-\uDBFF]/.test(text[at - 1]!));
  for (const entity of message.entities) {
    if (!isRecord(entity) || (entity.type !== "bold" && entity.type !== "code") || !isCount(entity.offset) || !isCount(entity.length)
      || entity.length === 0 || entity.offset < end || entity.offset + entity.length > message.text.length || !boundary(entity.offset) || !boundary(entity.offset + entity.length)) {
      throw new SetupsAnalyticsApiError("Некорректное форматирование текста сетапа");
    }
    end = entity.offset + entity.length;
    entities.push({ type: entity.type, offset: entity.offset, length: entity.length });
  }
  return { setup, telegramMessage: { text: message.text, entities } };
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

export type IssuedStudyAvailability = { status: "available" } | { status: "unavailable"; reason: string };
/** Saved research settings, never inferred from shared controls or forecastBest. */
export interface IssuedStudyObjective {
  version: 1; trendMode: "mean" | "forecast" | "blend"; trendBendDays: TrendBendDays;
  trendBlendLambda: number; forecast: ForecastRecipe | null;
}
export interface IssuedStudyProjection {
  availability: IssuedStudyAvailability;
  method: string | null;
  points: { atUnix: number; value: number | null }[];
  meanR: number | null; forecastR: number | null; decisionR: number | null;
  fittedOnIssuedData: boolean;
}
export interface IssuedStudyFilter { fromUnix?: number | null; toUnix?: number | null; q?: string | null }
export interface IssuedStudyDay {
  dayUnix: number; sumR: number | null; meanR: number | null; tpCount: number; slCount: number;
  closedCount: number; historicalClosedCount: number; issuedClosedCount: number;
}
/** Direct DTO for /study-expectancy/{id}/projection, never a post-study job.
 * GET reads a fixed saved snapshot and sealed curve without combined evaluation.
 * POST evaluates only that fingerprinted snapshot in RAM using the SAME recipe;
 * no browser aggregates, refresh, replay, reconstruction or market reads.
 * One open dialog owns <=3661 days / 10000 points per curve and releases on close. */
export interface IssuedStudyExpectancy {
  version: 1; studyId: number; sourceKind: "persisted_issued_setups"; retrospective: false;
  objective: IssuedStudyObjective;
  recommendedPair: { tpPct: number; slPct: number };
  originalSource: { studyIdentity: string; artifactId: number | null; artifactKey: string | null; revisionKey: "original"; observationCutoff: number | null };
  issuedSource: { availability: IssuedStudyAvailability; outcomePolicy: string; setupCount: number; closedCount: number | null; scientificDay: "raw_signal_close_utc"; observedDay: "entry_utc" };
  filter: IssuedStudyFilter; snapshot: SetupsAnalyticsSnapshot; startDayUnix: number;
  /** Server-owned UTC day sums/counts; timezone changes format, never regroup or fit. */
  originalDaily: IssuedStudyDay[]; daily: IssuedStudyDay[];
  originalProjection: IssuedStudyProjection;
  combinedProjection: IssuedStudyProjection;
}

function invalidIssuedStudy(): SetupsAnalyticsApiError {
  return new SetupsAnalyticsApiError("Несовместимый сохранённый график исследования: снимок, рецепт, источник или численные поля.");
}
function issuedAvailability(value: unknown): IssuedStudyAvailability {
  if (!isRecord(value)) throw invalidIssuedStudy();
  if (value.status === "available") return { status: "available" };
  if (value.status === "unavailable" && typeof value.reason === "string" && value.reason.length > 0 && value.reason.length <= 2000) return { status: "unavailable", reason: value.reason };
  throw invalidIssuedStudy();
}
function issuedObjective(value: unknown): IssuedStudyObjective {
  if (!isRecord(value) || value.version !== 1 || (value.trendMode !== "mean" && value.trendMode !== "forecast" && value.trendMode !== "blend")
    || !isTrendBendDays(value.trendBendDays) || !isFiniteNumber(value.trendBlendLambda) || value.trendBlendLambda < 0.05 || value.trendBlendLambda > 0.95) throw invalidIssuedStudy();
  let forecast: ForecastRecipe | null = null;
  if (value.forecast !== null) {
    try {
      const draft = restoreForecastRecipeDraft(value.forecast);
      if (draft === undefined) throw invalidIssuedStudy();
      forecast = buildForecastRecipe(draft);
    } catch { throw invalidIssuedStudy(); }
  }
  return { version: 1, trendMode: value.trendMode, trendBendDays: value.trendBendDays, trendBlendLambda: value.trendBlendLambda, forecast };
}
function issuedProjection(value: unknown, cutoff: number, objective: IssuedStudyObjective, fitted: boolean): IssuedStudyProjection {
  if (!isRecord(value) || value.fittedOnIssuedData !== fitted || !(value.method === null || (typeof value.method === "string" && value.method.length <= 128))
    || ![value.meanR, value.forecastR, value.decisionR].every((item) => item === null || isFiniteNumber(item))
    || !Array.isArray(value.points) || value.points.length > 10000) throw invalidIssuedStudy();
  const availability = issuedAvailability(value.availability);
  let previous = -Infinity;
  const points = value.points.map((point: unknown) => {
    if (!isRecord(point) || !isUnix(point.atUnix) || point.atUnix < previous || point.atUnix > cutoff || !(point.value === null || isFiniteNumber(point.value))) throw invalidIssuedStudy();
    previous = point.atUnix;
    return { atUnix: point.atUnix, value: point.value };
  });
  if (availability.status === "available" ? value.method === null || !isFiniteNumber(value.decisionR) || !points.some((point) => point.value !== null)
    || (objective.trendMode === "mean" ? value.method !== "mean" : objective.forecast?.method === "decayed_probability" ? value.method !== "decayed_probability" : !["trend", "ols", "linear", "p_spline"].includes(value.method))
    : points.length !== 0 || value.meanR !== null || value.forecastR !== null || value.decisionR !== null) throw invalidIssuedStudy();
  return { availability, method: value.method, points, meanR: value.meanR as number | null,
    forecastR: value.forecastR as number | null, decisionR: value.decisionR as number | null, fittedOnIssuedData: fitted };
}

function issuedFilter(value: unknown): IssuedStudyFilter {
  if (!isRecord(value) || ![value.fromUnix, value.toUnix].every((item) => item === undefined || item === null || isCount(item))
    || ((value.fromUnix == null) !== (value.toUnix == null)) || (isCount(value.fromUnix) && isCount(value.toUnix) && value.fromUnix >= value.toUnix)
    || !(value.q === undefined || value.q === null || (typeof value.q === "string" && value.q.length <= 200))) throw invalidIssuedStudy();
  return { fromUnix: value.fromUnix as number | null | undefined ?? null, toUnix: value.toUnix as number | null | undefined ?? null, q: value.q ?? null };
}
function issuedDays(value: unknown, cutoff: number): IssuedStudyDay[] {
  if (!Array.isArray(value) || value.length > 3661) throw invalidIssuedStudy();
  let previous = -Infinity;
  return value.map((point: unknown) => {
    if (!isRecord(point) || !isUnix(point.dayUnix) || point.dayUnix % 86400 !== 0 || point.dayUnix <= previous || point.dayUnix > cutoff || !isCount(point.closedCount)
      || !isCount(point.tpCount) || !isCount(point.slCount) || point.tpCount + point.slCount !== point.closedCount
      || !isCount(point.historicalClosedCount) || !isCount(point.issuedClosedCount) || point.historicalClosedCount + point.issuedClosedCount !== point.closedCount
      || !(point.sumR === null || isFiniteNumber(point.sumR)) || !(point.meanR === null || isFiniteNumber(point.meanR))
      || (point.closedCount === 0 ? point.meanR !== null || (point.sumR !== null && point.sumR !== 0)
        : point.sumR === null || point.meanR === null || Math.abs(point.sumR / point.closedCount - point.meanR) > 1e-9 * Math.max(1, Math.abs(point.meanR)))) throw invalidIssuedStudy();
    previous = point.dayUnix;
    return { dayUnix: point.dayUnix, sumR: point.sumR, meanR: point.meanR, closedCount: point.closedCount,
      tpCount: point.tpCount, slCount: point.slCount, historicalClosedCount: point.historicalClosedCount, issuedClosedCount: point.issuedClosedCount };
  });
}

export function parseIssuedStudyExpectancy(value: unknown, studyId: number, filter: IssuedStudyFilter = {}): IssuedStudyExpectancy {
  if (!isRecord(value) || value.version !== 1 || value.studyId !== studyId || value.sourceKind !== "persisted_issued_setups" || value.retrospective !== false
    || !isSetupsAnalyticsSnapshot(value.snapshot) || !isUnix(value.startDayUnix) || value.startDayUnix % 86400 !== 0
    || !isRecord(value.recommendedPair) || !isFiniteNumber(value.recommendedPair.tpPct) || value.recommendedPair.tpPct <= 0
    || !isFiniteNumber(value.recommendedPair.slPct) || value.recommendedPair.slPct <= 0
    || !isRecord(value.originalSource) || typeof value.originalSource.studyIdentity !== "string" || value.originalSource.studyIdentity.length > 256
    || !(value.originalSource.artifactId === null || isId(value.originalSource.artifactId)) || !(value.originalSource.artifactKey === null || (typeof value.originalSource.artifactKey === "string" && value.originalSource.artifactKey.length <= 256))
    || value.originalSource.revisionKey !== "original" || !(value.originalSource.observationCutoff === null || isCount(value.originalSource.observationCutoff))
    || !isRecord(value.issuedSource) || value.issuedSource.outcomePolicy !== "issued_open_entry_sl_first_10080m" || !isCount(value.issuedSource.setupCount)
    || !(value.issuedSource.closedCount === null || (isCount(value.issuedSource.closedCount) && value.issuedSource.closedCount <= value.issuedSource.setupCount))
    || value.issuedSource.scientificDay !== "raw_signal_close_utc" || value.issuedSource.observedDay !== "entry_utc") throw invalidIssuedStudy();
  const parsedFilter = issuedFilter(value.filter);
  if (JSON.stringify(parsedFilter) !== JSON.stringify(issuedFilter(filter))) throw invalidIssuedStudy();
  const objective = issuedObjective(value.objective);
  return { version: 1, studyId, sourceKind: "persisted_issued_setups", retrospective: false, objective,
    recommendedPair: { tpPct: value.recommendedPair.tpPct, slPct: value.recommendedPair.slPct }, filter: parsedFilter,
    snapshot: value.snapshot, startDayUnix: value.startDayUnix,
    originalSource: { studyIdentity: value.originalSource.studyIdentity, artifactId: value.originalSource.artifactId, artifactKey: value.originalSource.artifactKey,
      revisionKey: "original", observationCutoff: value.originalSource.observationCutoff },
    issuedSource: { availability: issuedAvailability(value.issuedSource.availability), outcomePolicy: value.issuedSource.outcomePolicy, setupCount: value.issuedSource.setupCount,
      closedCount: value.issuedSource.closedCount, scientificDay: "raw_signal_close_utc", observedDay: "entry_utc" },
    originalDaily: issuedDays(value.originalDaily, value.snapshot.observationCutoff), daily: issuedDays(value.daily, value.snapshot.observationCutoff),
    originalProjection: issuedProjection(value.originalProjection, value.snapshot.observationCutoff, objective, false),
    combinedProjection: issuedProjection(value.combinedProjection, value.snapshot.observationCutoff, objective, true) };
}

async function issuedStudyBody(response: Response): Promise<unknown> {
  if (response.status === 409) {
    const body: unknown = await response.json().catch(() => null);
    if (isRecord(body) && body.error === "setups_study_projection_snapshot_changed") throw new SetupsAnalyticsApiError("Снимок выпущенных сетапов изменился. Модель по всем данным недоступна; исходная модель сохранена. Откройте исследование заново для нового снимка.", 409);
    throw new SetupsAnalyticsApiError("Оценка модели сейчас недоступна: HTTP 409. Повторный запрос автоматически не запускается.", 409);
  }
  return readBody(response);
}
export async function fetchIssuedStudyExpectancy(studyId: number, filter: IssuedStudyFilter, signal: AbortSignal): Promise<IssuedStudyExpectancy> {
  const query = new URLSearchParams();
  if (filter.fromUnix != null) query.set("fromUnix", String(filter.fromUnix));
  if (filter.toUnix != null) query.set("toUnix", String(filter.toUnix));
  if (filter.q != null) query.set("q", filter.q);
  const suffix = query.size > 0 ? `?${query}` : "";
  const response = await apiFetch(`/v1/setups/analytics/study-expectancy/${studyId}/projection${suffix}`, { signal });
  const parsed = parseIssuedStudyExpectancy(await issuedStudyBody(response), studyId, filter);
  if (parsed.combinedProjection.availability.status === "available") throw invalidIssuedStudy();
  return parsed;
}
export async function evaluateIssuedStudyExpectancy(saved: IssuedStudyExpectancy, signal: AbortSignal): Promise<IssuedStudyExpectancy> {
  const filter = Object.fromEntries(Object.entries(saved.filter).filter(([, value]) => value != null));
  const response = await apiFetch(`/v1/setups/analytics/study-expectancy/${saved.studyId}/projection`, { method: "POST", signal,
    headers: { "content-type": "application/json" }, body: JSON.stringify({ filter, snapshot: saved.snapshot }) });
  const parsed = parseIssuedStudyExpectancy(await issuedStudyBody(response), saved.studyId, saved.filter);
  // No old response can change the sealed prefix, cohort or identity. Only the
  // diagnostic combined projection is allowed to differ between the two phases.
  const { combinedProjection: previousCombined, ...previousIdentity } = saved;
  const { combinedProjection: nextCombined, ...nextIdentity } = parsed;
  void previousCombined; void nextCombined;
  if (JSON.stringify(previousIdentity) !== JSON.stringify(nextIdentity)) throw invalidIssuedStudy();
  return parsed;
}
