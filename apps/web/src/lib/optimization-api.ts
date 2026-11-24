import { apiFetch } from "./platform-api";
import { parseStudyActualSummary, type StudyActualSummary } from "./study-actual-api";
import type { ForecastRecipe } from "./forecast-recipe";
import type { TrendBendDays, TrendMode as AnalyticsTrendMode } from "./analytics-api";
export type StudyTrendMode = AnalyticsTrendMode | "blend";
export function isTrendBlendLambda(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0.05 && value <= 0.95 && Math.round(value * 100) / 100 === value;
}

export type OptimizationValue = string | number | boolean | null;
export interface OptimizationAnalytics {
  clusterEnabled: boolean;
  clusterGapMinutes: number;
  timeShiftMinutes: number;
  priceChangePct: number | null;
}
export interface OptimizationSpec {
  algorithmId: string;
  algorithmVersion: string;
  algorithmParams: Record<string, string | number>;
  algorithmSearch: Record<string, (string | number)[]>;
  analyticsParams: OptimizationAnalytics;
  analyticsSearch: Partial<{ [K in keyof OptimizationAnalytics]: OptimizationAnalytics[K][] }>;
  instruments: { exchange: string; category: string; symbol: string }[];
  fromUnix: number;
  toUnix: number;
  direction: "short" | "long";
  setupMaturity: number;
  horizonMinutes: number;
  tpLevels: number[];
  slLevels: number[];
  minClosedPerCell: number;
  // Absent in legacy persisted studies; new forms explicitly send their threshold.
  minSetupsPerDay?: number;
  seed: number;
  maxTrials: number;
  maxSeconds: number;
  /** Absent legacy settings mean period mean / linear. */
  trendMode?: StudyTrendMode;
  trendBendDays?: TrendBendDays;
  trendBlendLambda?: number;
  minForecastExpectancy?: number;
  /** Explicit new recipe; missing legacy forecast/blend retains its original trend semantics. */
  forecast?: ForecastRecipe;
}
export interface OptimizationProgress {
  phase: string;
  evaluatedCandidates: number;
  scorableCandidates: number;
  maxTrials: number;
  elapsedSeconds: number;
  bestScore: number | null;
}
export interface OptimizationMatrixCell {
  tpPct: number; slPct: number; riskExpectancy: number | null; avgMovePct: number | null;
  closedCount: number; tpCount: number; slCount: number; unclosedCount: number;
  noDataCount: number; priceChangeFilteredCount: number;
  meanRiskExpectancy?: number | null;
  forecastRiskExpectancy?: number | null;
}
export interface OptimizationBest {
  score: number;
  meanScore?: number | null;
  trendMode?: StudyTrendMode;
  trendBendDays?: TrendBendDays;
  trendBlendLambda?: number;
  forecast?: ForecastRecipe;
  observationCutoff?: number;
  partial: boolean;
  params: Record<string, string | number>;
  analytics: Omit<OptimizationAnalytics, "clusterGapMinutes"> & { clusterGapMinutes: number | null; setupMaturity?: number | null; horizonMinutes?: number };
  matrix: OptimizationMatrixCell[];
  support?: {
    selectedSetupCount?: number;
    avgSetupsPerDay?: number | null;
    requiredSetupCount?: number | null;
  } | null;
  provenance: { optimizerVersion: string; seed: number; snapshotHash: string; capturedAt: string };
  conclusions: string[];
}
export interface OptimizationForecastBestPair {
  forecastR: number;
  meanR: number;
  tpPct: number;
  slPct: number;
}
export interface OptimizationRecommendedPair {
  tpPct: number;
  slPct: number;
  /** Selected-cell rawR: period mean, not the mean of the matrix. */
  meanR: number;
  forecastR: number | null;
  decisionR: number | null;
  /** Immutable original generation admission, not proof the profile is active now. */
  adaptiveAccepted: boolean | null;
}
export interface OptimizationStudy {
  id: number;
  origin?: "manual" | "continuous";
  continuousGenerationId?: number | null;
  continuousSlotId?: number | null;
  status: "queued" | "running" | "pausing" | "paused" | "cancel_requested" | "completed" | "cancelled" | "failed";
  /** Absent/null on legacy studies: never infer resumable support from status alone. */
  manualControl?: { revision: number; supportsPause: boolean } | null;
  spec: OptimizationSpec;
  progress: OptimizationProgress;
  result: { progress: OptimizationProgress; best: OptimizationBest | null; reason: "trial_budget" | "time_budget" | "space_exhausted" | "cancelled" } | null;
  error: { code: string; message: string } | null;
  /** Read-only server projection; absent on old APIs, null without saved forecast data. */
  forecastBest?: OptimizationForecastBestPair | null;
  /** Detail-only frozen selectedCell/adaptive handoff; never a fresh winner selection. */
  recommendedPair?: OptimizationRecommendedPair | null;
  /** Joined compact post-study job state; absent only on older servers. */
  actual?: StudyActualSummary | null;
  createdAt: string;
  updatedAt: string;
  events: { at: string; kind: string; message: string }[];
}

export type OptimizationStudySummary = Omit<OptimizationStudy, "result" | "events" | "recommendedPair"> & { bestScore: number | null };
export interface OptimizationStudyPage {
  studies: OptimizationStudySummary[];
  total: number;
  limit: number;
  offset: number;
}
export type OptimizationStudyAction = Pick<OptimizationStudy, "id" | "status" | "origin" | "manualControl">;

// Project mutation responses explicitly: a compact summary must not retain matrix/events.
export function summarizeOptimizationStudy(study: OptimizationStudy): OptimizationStudySummary {
  return { id: study.id, status: study.status, spec: study.spec, progress: study.progress, error: study.error,
    ...(study.origin ? { origin: study.origin } : {}),
    ...(study.continuousGenerationId !== undefined ? { continuousGenerationId: study.continuousGenerationId } : {}),
    ...(study.continuousSlotId !== undefined ? { continuousSlotId: study.continuousSlotId } : {}),
    ...(study.manualControl !== undefined ? { manualControl: study.manualControl } : {}),
    createdAt: study.createdAt, updatedAt: study.updatedAt, bestScore: study.result?.best?.score ?? study.progress.bestScore ?? null,
    forecastBest: study.forecastBest ?? null,
    ...(study.actual !== undefined ? { actual: study.actual === null ? null : parseStudyActualSummary(study.actual, study.id) } : {}) };
}

export const isActiveStudy = (study: Pick<OptimizationStudy, "status">): boolean =>
  ["queued", "running", "pausing", "paused", "cancel_requested"].includes(study.status);
export const isStudyTransitioning = (study: Pick<OptimizationStudy, "status">): boolean => isActiveStudy(study) && study.status !== "paused";
export const isContinuousStudy = (study: Pick<OptimizationStudy, "origin">): boolean => study.origin === "continuous";
export const canDeleteStudy = (study: OptimizationStudyAction): boolean =>
  ["completed", "cancelled", "failed"].includes(study.status) || (!isContinuousStudy(study) && study.status === "queued");
export const canStopStudy = (study: OptimizationStudyAction): boolean =>
  !isContinuousStudy(study) && ["queued", "running", "pausing", "paused"].includes(study.status);
export const canPauseStudy = (study: OptimizationStudyAction): boolean =>
  !isContinuousStudy(study) && study.status === "running" && study.manualControl?.supportsPause === true;
export const canPlayStudy = (study: OptimizationStudyAction): boolean =>
  !isContinuousStudy(study) && study.status === "paused" && study.manualControl?.supportsPause === true;

export class OptimizationApiError extends Error {
  constructor(message: string, readonly status: number, readonly activeStudyId?: number) { super(message); }
}

async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
  const response = await apiFetch(path, init);
  if (response.ok) return await response.json() as T;
  const detail = await response.json().catch(() => null) as { error?: unknown; message?: string; activeStudyId?: number; resource?: string; requested?: number; limit?: number } | null;
  const activeId = detail?.activeStudyId;
  let message = `Не удалось выполнить запрос оптимизации: HTTP ${response.status}`;
  if (response.status === 401) message = "Сессия истекла. Войдите заново перед изменением исследований.";
  else if (response.status === 409 && detail?.error === "continuous_history_referenced") message = "Нельзя удалить исследование: оно связано с адаптивным профилем или сохраняемой непрерывной цепочкой. Завершённая история сохранена.";
  else if (response.status === 409 && detail?.error === "continuous_owner_stop_required") message = "Непрерывное исследование нельзя остановить отдельно. Используйте общую остановку непрерывного процесса алгоритма.";
  else if (response.status === 409 && /\/(pause|play)$/.test(path)) message = "Состояние исследования изменилось. Обновите статус перед повтором команды.";
  else if (response.status === 409) message = init.method === "DELETE" ? "Нельзя удалить активное исследование. Сначала остановите его и дождитесь завершения."
    : activeId !== undefined ? `Исследование #${activeId} уже выполняется. Восстановлен его статус.` : "Версия алгоритма изменилась. Обновите страницу перед новым запуском.";
  else if (response.status === 404) message = "Исследование не найдено или уже удалено.";
  else if (response.status === 429) message = `Превышен ресурсный допуск${detail?.resource ? ` (${detail.resource}: ${String(detail.requested)} > ${String(detail.limit)})` : ""}. Уменьшите scope или бюджет исследования.`;
  else if (response.status === 400) {
    const reason = typeof detail?.message === "string" ? detail.message : typeof detail?.error === "string" ? detail.error : null;
    message = `Проверьте домены и совместимость параметров${reason ? `: ${reason.slice(0, 300)}` : ". Сервер отклонил запрос."}`;
  }
  throw new OptimizationApiError(message, response.status, activeId);
}

function parseStudyControl<T extends OptimizationStudyAction>(study: T, expectedId?: number): T {
  const control = study?.manualControl;
  if (!study || !Number.isSafeInteger(study.id) || study.id <= 0 || (expectedId !== undefined && study.id !== expectedId)
    || !["queued", "running", "pausing", "paused", "cancel_requested", "completed", "cancelled", "failed"].includes(study.status)
    || (control != null && (typeof control !== "object" || !Number.isSafeInteger(control.revision) || control.revision < 0 || typeof control.supportsPause !== "boolean"))) {
    throw new OptimizationApiError("Сервер вернул неподдерживаемый статус исследования. Обновите статус перед выполнением команд.", 502);
  }
  return study;
}

export async function listOptimizationStudies(algorithmId: string | undefined, signal: AbortSignal, limit = 1, offset = 0): Promise<OptimizationStudyPage> {
  const query = new URLSearchParams({ limit: String(limit), offset: String(offset) });
  if (algorithmId !== undefined) query.set("algorithmId", algorithmId);
  const page = await request<OptimizationStudyPage>(`/v1/optimization-studies?${query}`, { signal });
  return { ...page, studies: page.studies.map((study) => { parseStudyControl(study); return study.actual == null ? study : { ...study, actual: parseStudyActualSummary(study.actual, study.id) }; }) };
}
export const getOptimizationStudy = async (id: number, signal: AbortSignal): Promise<OptimizationStudy> =>
  parseStudyControl(await request<OptimizationStudy>(`/v1/optimization-studies/${id}`, { signal }), id);
export const createOptimizationStudy = async (spec: OptimizationSpec, signal: AbortSignal, idempotencyKey?: string): Promise<OptimizationStudy> =>
  parseStudyControl(await request<OptimizationStudy>("/v1/optimization-studies", { method: "POST", headers: { "content-type": "application/json", ...(idempotencyKey ? { "Idempotency-Key": idempotencyKey } : {}) }, body: JSON.stringify(spec), signal }));
export const cancelOptimizationStudy = async (id: number, signal: AbortSignal): Promise<OptimizationStudy> =>
  parseStudyControl(await request<OptimizationStudy>(`/v1/optimization-studies/${id}/cancel`, { method: "POST", signal }), id);
async function manualCommand(study: OptimizationStudyAction, action: "pause" | "play", signal: AbortSignal): Promise<OptimizationStudy> {
  if (!(action === "pause" ? canPauseStudy(study) : canPlayStudy(study)) || !study.manualControl) throw new OptimizationApiError("Команда исследования недоступна. Обновите статус.", 409);
  return parseStudyControl(await request<OptimizationStudy>(`/v1/optimization-studies/${study.id}/${action}`, {
    method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ expectedControlRevision: study.manualControl.revision }), signal,
  }), study.id);
}
export const pauseOptimizationStudy = (study: OptimizationStudyAction, signal: AbortSignal): Promise<OptimizationStudy> => manualCommand(study, "pause", signal);
export const playOptimizationStudy = (study: OptimizationStudyAction, signal: AbortSignal): Promise<OptimizationStudy> => manualCommand(study, "play", signal);
export const deleteOptimizationStudy = (id: number, signal: AbortSignal): Promise<{ deleted: true; id: number }> =>
  request(`/v1/optimization-studies/${id}`, { method: "DELETE", signal });

export async function getOptimizationDraft(algorithmId: string, signal: AbortSignal): Promise<unknown> {
  const result = await request<{ algorithmId: string; value?: unknown }>(`/v1/optimization-drafts/${encodeURIComponent(algorithmId)}`, { signal });
  return result.value ?? null;
}

export async function putOptimizationDraft(algorithmId: string, value: object): Promise<void> {
  await request(`/v1/optimization-drafts/${encodeURIComponent(algorithmId)}`, {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ value }),
  });
}
