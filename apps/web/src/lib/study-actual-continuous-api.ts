import { apiFetch } from "./platform-api";

export interface StudyActualContinuous {
  algorithmId: string; enabled: boolean; fromDate: string; fromBoundary: string; timeZone: string; revision: number;
  cursorStudyId: number | null; cursorExecutionStartedAt: string | null; nextDueAt: string; lastSkipReason: string | null;
  queue: { manualQueued: number; automaticQueued: number; running: { studyId: number; jobId: string; status: "running"; algorithmId: string; requestOrigin: "manual" | "automatic" } | null };
}
export class StudyActualContinuousApiError extends Error {
  constructor(message: string, readonly status?: number) { super(message); }
}
const record = (value: unknown): value is Record<string, unknown> => value !== null && typeof value === "object" && !Array.isArray(value);
const count = (value: unknown): value is number => typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
const timestamp = (value: unknown): value is string => typeof value === "string" && value.length <= 64
  && /(?:Z|[+-]\d{2}:\d{2})$/.test(value) && Number.isFinite(Date.parse(value));
export function isStudyActualDate(value: string): boolean {
  return /^\d{4}-\d{2}-\d{2}$/.test(value) && value >= "1970-01-01" && value <= "9999-12-31"
    && Number.isFinite(Date.parse(`${value}T00:00:00Z`)) && new Date(`${value}T00:00:00Z`).toISOString().slice(0, 10) === value;
}
export function parseStudyActualContinuous(value: unknown, algorithmId: string): StudyActualContinuous {
  const invalid = () => new StudyActualContinuousApiError("Некорректный ответ настройки непрерывного пересчёта матожидания.");
  if (!record(value) || value.algorithmId !== algorithmId || typeof value.enabled !== "boolean" || typeof value.fromDate !== "string"
    || !isStudyActualDate(value.fromDate) || !timestamp(value.fromBoundary) || !count(value.revision) || value.revision < 1
    || !(value.cursorStudyId === null || (count(value.cursorStudyId) && value.cursorStudyId > 0))
    || !(value.cursorExecutionStartedAt === null || timestamp(value.cursorExecutionStartedAt)) || !timestamp(value.nextDueAt)
    || !(value.lastSkipReason === null || (typeof value.lastSkipReason === "string" && value.lastSkipReason.length <= 300))
    || typeof value.timeZone !== "string" || value.timeZone.length > 128
    || !record(value.queue) || !count(value.queue.manualQueued) || !count(value.queue.automaticQueued)) throw invalid();
  try { new Intl.DateTimeFormat("en-US", { timeZone: value.timeZone }); } catch { throw invalid(); }
  const running = value.queue.running;
  if (running !== null && (!record(running) || !count(running.studyId) || running.studyId < 1 || typeof running.algorithmId !== "string"
    || running.algorithmId.length > 128 || typeof running.jobId !== "string" || running.jobId.length < 1 || running.jobId.length > 128
    || running.status !== "running" || (running.requestOrigin !== "manual" && running.requestOrigin !== "automatic"))) throw invalid();
  // Retain only bounded metadata, never an owner identifier or opaque payload.
  return { algorithmId, enabled: value.enabled, fromDate: value.fromDate, fromBoundary: value.fromBoundary,
    timeZone: value.timeZone, revision: value.revision, cursorStudyId: value.cursorStudyId, cursorExecutionStartedAt: value.cursorExecutionStartedAt,
    nextDueAt: value.nextDueAt, lastSkipReason: value.lastSkipReason, queue: { manualQueued: value.queue.manualQueued, automaticQueued: value.queue.automaticQueued,
      running: running === null ? null : { studyId: running.studyId as number, jobId: running.jobId as string, status: "running",
        algorithmId: running.algorithmId as string, requestOrigin: running.requestOrigin as "manual" | "automatic" } } };
}
export async function studyActualContinuous(algorithmId: string, signal: AbortSignal, update?: { enabled: boolean; fromDate: string; timeZone: string; revision: number }): Promise<StudyActualContinuous> {
  const response = await apiFetch(`/v1/optimization-studies/actual/continuous?${new URLSearchParams({ algorithmId })}`, {
    signal, ...(update === undefined ? {} : { method: "PUT", headers: { "content-type": "application/json" }, body: JSON.stringify(update) }),
  });
  if (!response.ok) throw new StudyActualContinuousApiError(response.status === 409
    ? "Настройка изменена в другом клиенте. Изменение не подтверждено; загружаем сохранённое состояние."
    : response.status === 401 ? "Сессия истекла. Войдите заново."
      : `Настройка непрерывного пересчёта недоступна: HTTP ${response.status}.`, response.status);
  return parseStudyActualContinuous(await response.json(), algorithmId);
}
