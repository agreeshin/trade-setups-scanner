import { apiFetch } from "./platform-api";

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

async function requestSummary(studyId: number, init: RequestInit, refresh: boolean): Promise<StudyActualSummary> {
  const response = await apiFetch(`/v1/optimization-studies/${studyId}/actual${refresh ? "/refresh" : ""}`, init);
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
  return parseStudyActualSummary(body, studyId);
}

export function getStudyActualSummary(studyId: number, signal: AbortSignal): Promise<StudyActualSummary> {
  return requestSummary(studyId, { signal }, false);
}
export function refreshStudyActual(studyId: number, signal: AbortSignal): Promise<StudyActualSummary> {
  return requestSummary(studyId, { method: "POST", headers: { "content-type": "application/json" }, body: "{}", signal }, true);
}
export const isStudyActualPending = (actual: Pick<StudyActualSummary, "status"> | null | undefined): boolean => actual?.status === "queued" || actual?.status === "running";
