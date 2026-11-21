import { apiFetch } from "./platform-api";
import { buildForecastRecipe, isTrendBendDays, restoreForecastRecipeDraft, type ForecastRecipe } from "./forecast-recipe";
import type { TrendBendDays } from "./analytics-api";
import type { SetupsAnalyticsSetup, SetupsAnalyticsStatus } from "./setups-analytics-api";

export type StudyActualStatus = "not_calculated" | "queued" | "running" | "completed" | "failed";
export type StudyActualStage = "queued" | "acquiring" | "replaying" | "evaluating" | "publishing" | "completed" | "failed";
export interface StudyActualCounts {
  total: number; closed: number; tp: number; sl: number; gateFailed: number; unclosed: number; noData: number;
}
/** Only compact metadata belongs to a table row; reports and setup pages belong to an open dialog. */
export interface StudyActualSummary {
  studyId: number;
  eligibility: { eligible: boolean; reason: string | null };
  jobId: string | null;
  revision: number;
  status: StudyActualStatus;
  stage: StudyActualStage | null;
  observedThroughUnix: number | null;
  counts: StudyActualCounts | null;
  meanR: number | null;
  error: { code: string; stage: string; message: string } | null;
}
export type StudyActualAvailability = { status: "available" } | { status: "unavailable"; reason: string };
export type StudyActualNumber = { status: "available"; value: number } | { status: "unavailable"; reason: string };
export interface OriginalObjectiveDTO {
  version: 1; trendMode: "mean" | "forecast" | "blend"; trendBendDays: TrendBendDays;
  trendBlendLambda: number; forecast: ForecastRecipe | null;
}
export interface StudyActualDailyPoint {
  dayUnix: number; plotAtUnix: number; meanR: number | null; closedCount: number; tpCount: number; slCount: number;
}
export interface StudyActualDisplayDay {
  dayUnix: number; date: string; sumR: number | null; meanR: number | null; closedCount: number; tpCount: number; slCount: number;
}
export interface StudyActualDisplayQuality {
  availability: StudyActualAvailability; fidelity: string; complete: boolean; reason?: string;
  /** Opaque, bounded provenance; never interpreted as evidence of available outcomes. */
  sourceIdentity?: unknown;
  rawReplayAvailable?: boolean;
}
export interface StudyActualDisplay {
  timeZone: string; startDayUnix: number; daily: StudyActualDisplayDay[];
  historical: StudyActualDisplayQuality; actual: StudyActualDisplayQuality;
}
export interface StudyActualReport {
  sourceArtifactId: number | null; retrospective: true; executionStartedAt: string; observedThroughUnix: number;
  historical: { availability: StudyActualAvailability; daily: StudyActualDailyPoint[] };
  actual: { daily: StudyActualDailyPoint[] };
  originalObjective: OriginalObjectiveDTO | null;
  projection: { availability: StudyActualAvailability; method: string | null; points: { atUnix: number; value: number | null }[]; fittedOnPostStudyData: false };
  originalEndpoint: { meanR: StudyActualNumber; forecastR: StudyActualNumber; decisionR: StudyActualNumber; atUnix: number | null };
  /** Additive read-only grouping; absent only on the unchanged legacy GET path. */
  display?: StudyActualDisplay;
}
export interface StudyActualDetail extends StudyActualSummary {
  report: StudyActualReport | null; originalReport?: StudyActualReport | null; originalReportUnavailableReason?: string | null;
}
export type StudyActualSetup = Omit<SetupsAnalyticsSetup, "slotId"> & { slotId: number | null };
export interface StudyActualSetupPage {
  studyId: number; jobId: string; revision: number; retrospective: true; total: number; limit: number; offset: number; setups: StudyActualSetup[];
}
export const STUDY_ACTUAL_CHANGED = "study-actual-changed";
export const STUDY_ACTUAL_PAGE_SIZE = 50;

const record = (value: unknown): value is Record<string, unknown> => value !== null && typeof value === "object" && !Array.isArray(value);
const count = (value: unknown): value is number => typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
const finite = (value: unknown): value is number => typeof value === "number" && Number.isFinite(value);
const statuses: readonly unknown[] = ["not_calculated", "queued", "running", "completed", "failed"];
const stages: readonly unknown[] = ["queued", "acquiring", "replaying", "evaluating", "publishing", "completed", "failed"];

export class StudyActualApiError extends Error {
  constructor(message: string, readonly status?: number, readonly code?: string, readonly current: StudyActualSummary | null = null) { super(message); }
}

export function parseStudyActualSummary(value: unknown, studyId: number): StudyActualSummary {
  const invalid = () => new StudyActualApiError("Некорректный ответ расчёта матожидания после исследования.");
  if (!record(value) || (value.studyId !== undefined && value.studyId !== studyId)
    || !record(value.eligibility) || typeof value.eligibility.eligible !== "boolean"
    || !(value.eligibility.reason === null || typeof value.eligibility.reason === "string")
    || !(value.jobId === null || typeof value.jobId === "string") || !count(value.revision)
    || !statuses.includes(value.status) || !(stages.includes(value.stage) || (value.status === "not_calculated" && value.stage === null))
    || !(value.observedThroughUnix === null || count(value.observedThroughUnix))
    || !(value.meanR === null || finite(value.meanR))) throw invalid();
  const rawCounts = value.counts;
  let counts: StudyActualCounts | null = null;
  if (rawCounts !== null) {
    if (!record(rawCounts) || !count(rawCounts.total) || !count(rawCounts.closed) || !count(rawCounts.tp)
      || !count(rawCounts.sl) || !count(rawCounts.gateFailed) || !count(rawCounts.unclosed) || !count(rawCounts.noData)
      || rawCounts.closed > rawCounts.total || (rawCounts.closed === 0 && value.meanR !== null)) throw invalid();
    counts = { total: rawCounts.total, closed: rawCounts.closed, tp: rawCounts.tp, sl: rawCounts.sl,
      gateFailed: rawCounts.gateFailed, unclosed: rawCounts.unclosed, noData: rawCounts.noData };
  }
  let error: StudyActualSummary["error"] = null;
  if (value.error !== null && value.error !== undefined) {
    if (!record(value.error) || typeof value.error.code !== "string" || typeof value.error.stage !== "string" || typeof value.error.message !== "string") throw invalid();
    error = { code: value.error.code, stage: value.error.stage, message: value.error.message };
  }
  return { studyId, eligibility: { eligible: value.eligibility.eligible, reason: value.eligibility.reason },
    jobId: value.jobId, revision: value.revision, status: value.status as StudyActualStatus, stage: value.stage as StudyActualStage | null,
    observedThroughUnix: value.observedThroughUnix, counts, meanR: value.meanR, error };
}

async function requestBody(studyId: number, init: RequestInit, suffix = ""): Promise<unknown> {
  const response = await apiFetch(`/v1/optimization-studies/${studyId}/actual${suffix}`, init);
  const body: unknown = await response.json().catch(() => null);
  if (!response.ok) {
    const code = record(body) && typeof body.error === "string" ? body.error : "";
    const reason = record(body) && typeof body.reason === "string" ? body.reason : null;
    const currentBody = record(body) ? body.actual : undefined;
    const current = code === "study_actual_busy" && currentBody !== undefined ? parseStudyActualSummary(currentBody, studyId) : null;
    const message = response.status === 401 ? "Сессия истекла. Войдите заново."
      : response.status === 404 ? "Исследование недоступно или уже удалено."
      : response.status === 422 ? `Расчёт недоступен для этого исследования${reason ? `: ${reason}` : "."}`
      : response.status === 429 ? "Недостаточно вычислительных ресурсов. Повторите запуск вручную позже."
      : code === "study_actual_busy" ? "Расчёт этого исследования уже выполняется."
      : code === "study_actual_revision_changed" ? "Результат исследования обновился. Загрузите текущую ревизию."
      : `Расчёт после исследования недоступен: HTTP ${response.status}`;
    throw new StudyActualApiError(message, response.status, code, current);
  }
  return body;
}

export async function getStudyActualSummary(studyId: number, signal: AbortSignal): Promise<StudyActualSummary> {
  return parseStudyActualSummary(await requestBody(studyId, { signal }), studyId);
}
export async function refreshStudyActual(studyId: number, signal: AbortSignal): Promise<StudyActualSummary> {
  const actual = parseStudyActualSummary(await requestBody(studyId, { method: "POST", headers: { "content-type": "application/json" }, body: "{}", signal }, "/refresh"), studyId);
  // Ephemeral invalidation only, never a browser report cache. Open views then read persisted state.
  window.dispatchEvent(new CustomEvent(STUDY_ACTUAL_CHANGED, { detail: actual }));
  return actual;
}
export const isStudyActualPending = (actual: Pick<StudyActualSummary, "status"> | null | undefined): boolean => actual?.status === "queued" || actual?.status === "running";

function invalidReport(): StudyActualApiError { return new StudyActualApiError("Некорректный сохранённый отчёт после исследования."); }
function availability(value: unknown): StudyActualAvailability {
  if (!record(value)) throw invalidReport();
  if (value.status === "available") return { status: "available" };
  if (value.status === "unavailable" && typeof value.reason === "string") return { status: "unavailable", reason: value.reason };
  throw invalidReport();
}
function availableNumber(value: unknown): StudyActualNumber {
  const state = availability(value);
  if (state.status === "unavailable") return state;
  if (!record(value) || !finite(value.value)) throw invalidReport();
  return { status: "available", value: value.value };
}
function dailyPoints(value: unknown): StudyActualDailyPoint[] {
  if (!Array.isArray(value) || value.length > 10_000) throw invalidReport();
  return value.map((point: unknown) => {
    if (!record(point) || !count(point.dayUnix) || !finite(point.plotAtUnix) || !(point.meanR === null || finite(point.meanR))
      || !count(point.closedCount) || !count(point.tpCount) || !count(point.slCount)) throw invalidReport();
    return { dayUnix: point.dayUnix, plotAtUnix: point.plotAtUnix, meanR: point.meanR, closedCount: point.closedCount, tpCount: point.tpCount, slCount: point.slCount };
  });
}
/** ExpectancyObjective has serde(rename_all="camelCase"); no frontend recipe defaults or model fit. */
export function parseOriginalObjective(value: unknown): OriginalObjectiveDTO | null {
  if (value === null) return null;
  if (!record(value) || value.version !== 1 || !["mean", "forecast", "blend"].includes(String(value.trendMode))
    || !isTrendBendDays(value.trendBendDays) || !finite(value.trendBlendLambda)
    || value.trendBlendLambda < 0.05 || value.trendBlendLambda > 0.95) throw invalidReport();
  let forecast: ForecastRecipe | null = null;
  if (value.forecast !== null) {
    const draft = restoreForecastRecipeDraft(value.forecast);
    if (draft === undefined) throw invalidReport();
    forecast = buildForecastRecipe(draft);
  }
  return { version: 1, trendMode: value.trendMode as OriginalObjectiveDTO["trendMode"], trendBendDays: value.trendBendDays,
    trendBlendLambda: value.trendBlendLambda, forecast };
}
function canonicalTimeZone(value: string): string {
  try { return new Intl.DateTimeFormat("en-US", { timeZone: value }).resolvedOptions().timeZone; }
  catch { throw invalidReport(); }
}
function displayQuality(value: unknown): StudyActualDisplayQuality {
  if (!record(value) || typeof value.fidelity !== "string" || value.fidelity.length > 128 || typeof value.complete !== "boolean"
    || !(value.reason === undefined || (typeof value.reason === "string" && value.reason.length <= 2000))
    || !(value.rawReplayAvailable === undefined || typeof value.rawReplayAvailable === "boolean")) throw invalidReport();
  if (value.sourceIdentity !== undefined && JSON.stringify(value.sourceIdentity).length > 4096) throw invalidReport();
  return { availability: availability(value.availability), fidelity: value.fidelity, complete: value.complete,
    ...(value.reason === undefined ? {} : { reason: value.reason }),
    ...(value.sourceIdentity === undefined ? {} : { sourceIdentity: value.sourceIdentity }),
    ...(value.rawReplayAvailable === undefined ? {} : { rawReplayAvailable: value.rawReplayAvailable }) };
}
function parseDisplay(value: unknown, executionStartedAt: string, expectedTimeZone?: string): StudyActualDisplay {
  if (!record(value) || typeof value.timeZone !== "string" || value.timeZone.length > 128 || !count(value.startDayUnix)
    || !Array.isArray(value.daily) || value.daily.length > 3661) throw invalidReport();
  const zone = canonicalTimeZone(value.timeZone);
  if (expectedTimeZone !== undefined && zone !== canonicalTimeZone(expectedTimeZone)) throw invalidReport();
  const formatter = new Intl.DateTimeFormat("en-US", { timeZone: zone, year: "numeric", month: "2-digit", day: "2-digit" });
  const dateKey = (unix: number): string => {
    if (!Number.isFinite(new Date(unix * 1000).getTime())) throw invalidReport();
    const parts = formatter.formatToParts(unix * 1000);
    return ["year", "month", "day"].map((part) => parts.find((item) => item.type === part)?.value).join("-");
  };
  const start = Date.parse(executionStartedAt) / 1000;
  if (value.startDayUnix > start || dateKey(value.startDayUnix) !== dateKey(start)
    || dateKey(value.startDayUnix - 1) === dateKey(value.startDayUnix)) throw invalidReport();
  let previous = -Infinity;
  const daily = value.daily.map((point: unknown): StudyActualDisplayDay => {
    if (!record(point) || !count(point.dayUnix) || point.dayUnix <= previous || typeof point.date !== "string"
      || !/^\d{4}-\d{2}-\d{2}$/.test(point.date) || point.date !== dateKey(point.dayUnix) || dateKey(point.dayUnix - 1) === point.date
      || !(point.sumR === null || finite(point.sumR)) || !(point.meanR === null || finite(point.meanR))
      || !count(point.closedCount) || !count(point.tpCount) || !count(point.slCount) || point.closedCount !== point.tpCount + point.slCount
      || (point.closedCount === 0 ? point.meanR !== null || (point.sumR !== null && point.sumR !== 0)
        : point.sumR === null || point.meanR === null || Math.abs(point.sumR / point.closedCount - point.meanR) > 1e-9 * Math.max(1, Math.abs(point.meanR)))) throw invalidReport();
    previous = point.dayUnix;
    return { dayUnix: point.dayUnix, date: point.date, sumR: point.sumR, meanR: point.meanR,
      closedCount: point.closedCount, tpCount: point.tpCount, slCount: point.slCount };
  });
  if (daily.length > 1 && daily.at(-1)!.dayUnix - daily[0]!.dayUnix > 3661 * 86400) throw invalidReport();
  return { timeZone: value.timeZone, startDayUnix: value.startDayUnix, daily,
    historical: displayQuality(value.historical), actual: displayQuality(value.actual) };
}
function parseReport(value: unknown, expectedTimeZone?: string): StudyActualReport {
  if (!record(value) || !(value.sourceArtifactId === null || (count(value.sourceArtifactId) && value.sourceArtifactId > 0))
    || value.retrospective !== true || typeof value.executionStartedAt !== "string" || !Number.isFinite(Date.parse(value.executionStartedAt))
    || !count(value.observedThroughUnix) || !record(value.historical) || !record(value.actual) || !record(value.projection)
    || value.projection.fittedOnPostStudyData !== false || !(value.projection.method === null || typeof value.projection.method === "string")
    || !Array.isArray(value.projection.points) || value.projection.points.length > 10_000 || !record(value.originalEndpoint)
    || !(value.originalEndpoint.atUnix === null || finite(value.originalEndpoint.atUnix))) throw invalidReport();
  const points = value.projection.points.map((point: unknown) => {
    if (!record(point) || !finite(point.atUnix) || !(point.value === null || finite(point.value))) throw invalidReport();
    return { atUnix: point.atUnix, value: point.value };
  });
  if (expectedTimeZone !== undefined && value.display === undefined) throw invalidReport();
  return { sourceArtifactId: value.sourceArtifactId, retrospective: true, executionStartedAt: value.executionStartedAt, observedThroughUnix: value.observedThroughUnix,
    historical: { availability: availability(value.historical.availability), daily: dailyPoints(value.historical.daily) },
    actual: { daily: dailyPoints(value.actual.daily) }, originalObjective: parseOriginalObjective(value.originalObjective),
    projection: { availability: availability(value.projection.availability), method: value.projection.method, points, fittedOnPostStudyData: false },
    originalEndpoint: { meanR: availableNumber(value.originalEndpoint.meanR), forecastR: availableNumber(value.originalEndpoint.forecastR), decisionR: availableNumber(value.originalEndpoint.decisionR), atUnix: value.originalEndpoint.atUnix },
    ...(value.display === undefined ? {} : { display: parseDisplay(value.display, value.executionStartedAt, expectedTimeZone) }) };
}
export async function getStudyActual(studyId: number, signal: AbortSignal, timeZone?: string): Promise<StudyActualDetail> {
  const query = timeZone === undefined ? "" : `?${new URLSearchParams({ timeZone })}`;
  const body = await requestBody(studyId, { signal }, query);
  const summary = parseStudyActualSummary(body, studyId);
  if (!record(body) || (timeZone !== undefined && !("originalReport" in body))
    || !(body.originalReportUnavailableReason === undefined || body.originalReportUnavailableReason === null
      || (typeof body.originalReportUnavailableReason === "string" && body.originalReportUnavailableReason.length <= 2000))
    || (summary.status === "completed" && (body.report == null || summary.jobId === null || summary.revision < 1 || summary.counts === null))) throw invalidReport();
  return { ...summary, report: summary.status === "completed" ? parseReport(body.report, timeZone) : null,
    ...("originalReport" in body ? { originalReport: body.originalReport === null ? null : parseReport(body.originalReport, timeZone) } : {}),
    ...(body.originalReportUnavailableReason === undefined ? {} : { originalReportUnavailableReason: body.originalReportUnavailableReason }) };
}

function parseSetup(value: unknown, studyId: number): StudyActualSetup {
  const statuses: readonly unknown[] = ["tp", "sl", "unclosed", "no_data", "gate_failed"];
  if (!record(value) || !count(value.id) || value.id < 1 || value.studyId !== studyId || typeof value.symbol !== "string"
    || typeof value.exchange !== "string" || !(value.slotId === null || (count(value.slotId) && value.slotId > 0))
    || !(value.direction === "short" || value.direction === "long") || !count(value.setupTimeUnix) || !statuses.includes(value.status)
    || ![value.entryTimeUnix, value.entryPrice, value.gateThreshold, value.tpPrice, value.slPrice, value.closeTimeUnix, value.closePrice, value.riskReturn].every((field) => field === null || finite(field))) throw invalidReport();
  return { id: value.id, studyId, symbol: value.symbol, exchange: value.exchange, slotId: value.slotId, direction: value.direction,
    setupTimeUnix: value.setupTimeUnix, entryTimeUnix: value.entryTimeUnix as number | null, entryPrice: value.entryPrice as number | null,
    gateThreshold: value.gateThreshold as number | null, tpPrice: value.tpPrice as number | null, slPrice: value.slPrice as number | null,
    closeTimeUnix: value.closeTimeUnix as number | null, closePrice: value.closePrice as number | null,
    riskReturn: value.riskReturn as number | null, status: value.status as SetupsAnalyticsStatus };
}
export async function getStudyActualSetups(studyId: number, revision: number, jobId: string, offset: number, signal: AbortSignal): Promise<StudyActualSetupPage> {
  const query = new URLSearchParams({ revision: String(revision), offset: String(offset), limit: String(STUDY_ACTUAL_PAGE_SIZE) });
  const body = await requestBody(studyId, { signal }, `/setups?${query}`);
  if (!record(body) || body.studyId !== studyId || body.revision !== revision || body.jobId !== jobId) {
    throw new StudyActualApiError("Результат исследования обновился. Загрузите текущую ревизию.", 409, "study_actual_revision_changed");
  }
  if (body.retrospective !== true || !count(body.total) || body.limit !== STUDY_ACTUAL_PAGE_SIZE || body.offset !== offset
    || !Array.isArray(body.setups) || body.setups.length > STUDY_ACTUAL_PAGE_SIZE || body.setups.length > body.total) throw invalidReport();
  return { studyId, jobId, revision, retrospective: true, total: body.total, limit: STUDY_ACTUAL_PAGE_SIZE, offset, setups: body.setups.map((setup: unknown) => parseSetup(setup, studyId)) };
}
