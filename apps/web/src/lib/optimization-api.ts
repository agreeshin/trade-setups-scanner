import { apiFetch } from "./platform-api";

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
}
export interface OptimizationBest {
  score: number;
  partial: boolean;
  params: Record<string, string | number>;
  analytics: Omit<OptimizationAnalytics, "clusterGapMinutes"> & { clusterGapMinutes: number | null };
  matrix: OptimizationMatrixCell[];
  support?: {
    selectedSetupCount?: number;
    avgSetupsPerDay?: number | null;
    requiredSetupCount?: number | null;
  } | null;
  provenance: { optimizerVersion: string; seed: number; snapshotHash: string; capturedAt: string };
  conclusions: string[];
}
export interface OptimizationStudy {
  id: number;
  status: "queued" | "running" | "cancel_requested" | "completed" | "cancelled" | "failed";
  spec: OptimizationSpec;
  progress: OptimizationProgress;
  result: { progress: OptimizationProgress; best: OptimizationBest | null; reason: "trial_budget" | "time_budget" | "space_exhausted" | "cancelled" } | null;
  error: { code: string; message: string } | null;
  createdAt: string;
  updatedAt: string;
  events: { at: string; kind: string; message: string }[];
}

export type OptimizationStudySummary = Omit<OptimizationStudy, "result" | "events"> & { bestScore: number | null };
export interface OptimizationStudyPage {
  studies: OptimizationStudySummary[];
  total: number;
  limit: number;
  offset: number;
}
export type OptimizationStudyAction = Pick<OptimizationStudy, "id" | "status">;

// Project mutation responses explicitly: a compact summary must not retain matrix/events.
export function summarizeOptimizationStudy(study: OptimizationStudy): OptimizationStudySummary {
  return { id: study.id, status: study.status, spec: study.spec, progress: study.progress, error: study.error,
    createdAt: study.createdAt, updatedAt: study.updatedAt, bestScore: study.result?.best?.score ?? study.progress.bestScore ?? null };
}

export const isActiveStudy = (study: Pick<OptimizationStudy, "status">): boolean =>
  study.status === "queued" || study.status === "running" || study.status === "cancel_requested";

export class OptimizationApiError extends Error {
  constructor(message: string, readonly status: number, readonly activeStudyId?: number) { super(message); }
}

async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
  const response = await apiFetch(path, init);
  if (response.ok) return await response.json() as T;
  const detail = await response.json().catch(() => null) as { error?: unknown; message?: string; activeStudyId?: number; resource?: string; requested?: number; limit?: number } | null;
  const activeId = detail?.activeStudyId;
  let message = `Не удалось выполнить запрос оптимизации: HTTP ${response.status}`;
  if (response.status === 409) message = init.method === "DELETE" ? "Нельзя удалить активное исследование. Сначала остановите его и дождитесь завершения."
    : activeId !== undefined ? `Исследование #${activeId} уже выполняется. Восстановлен его статус.` : "Версия алгоритма изменилась. Обновите страницу перед новым запуском.";
  else if (response.status === 404) message = "Исследование не найдено или уже удалено.";
  else if (response.status === 429) message = `Превышен ресурсный допуск${detail?.resource ? ` (${detail.resource}: ${String(detail.requested)} > ${String(detail.limit)})` : ""}. Уменьшите scope или бюджет исследования.`;
  else if (response.status === 400) {
    const reason = typeof detail?.message === "string" ? detail.message : typeof detail?.error === "string" ? detail.error : null;
    message = `Проверьте домены и совместимость параметров${reason ? `: ${reason.slice(0, 300)}` : ". Сервер отклонил запрос."}`;
  }
  throw new OptimizationApiError(message, response.status, activeId);
}

export async function listOptimizationStudies(algorithmId: string | undefined, signal: AbortSignal, limit = 1, offset = 0): Promise<OptimizationStudyPage> {
  const query = new URLSearchParams({ limit: String(limit), offset: String(offset) });
  if (algorithmId !== undefined) query.set("algorithmId", algorithmId);
  return request(`/v1/optimization-studies?${query}`, { signal });
}
export const getOptimizationStudy = (id: number, signal: AbortSignal): Promise<OptimizationStudy> =>
  request(`/v1/optimization-studies/${id}`, { signal });
export const createOptimizationStudy = (spec: OptimizationSpec, signal: AbortSignal): Promise<OptimizationStudy> =>
  request("/v1/optimization-studies", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(spec), signal });
export const cancelOptimizationStudy = (id: number, signal: AbortSignal): Promise<OptimizationStudy> =>
  request(`/v1/optimization-studies/${id}/cancel`, { method: "POST", signal });
export const deleteOptimizationStudy = (id: number, signal: AbortSignal): Promise<{ deleted: true; id: number }> =>
  request(`/v1/optimization-studies/${id}`, { method: "DELETE", signal });
