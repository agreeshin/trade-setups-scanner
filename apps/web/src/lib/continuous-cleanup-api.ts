import { apiFetch } from "./platform-api";

export interface ContinuousCleanupCounts {
  studies: number; generations: number; profiles: number; checkpoints: number;
  notificationJobs: number; processesReset: number;
}
export interface ContinuousCleanupRetained {
  runs: number; batches: number; qualityIntervals: number; analyticsResults: number;
  liveSetups: number; configuredProcesses: number; configuredSlots: number;
}
export interface ContinuousCleanupPreview {
  eligible: boolean; blockers: string[]; fingerprint: string;
  counts: ContinuousCleanupCounts; retained: ContinuousCleanupRetained;
}
export interface ContinuousCleanupResult { deleted: ContinuousCleanupCounts; retained: ContinuousCleanupRetained }
export class ContinuousCleanupApiError extends Error {
  constructor(message: string, readonly status?: number, readonly preview?: ContinuousCleanupPreview) { super(message); }
}

function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : null;
}
function counters(value: unknown, keys: string[]): boolean {
  const source = record(value);
  return source !== null && keys.every((key) => typeof source[key] === "number" && Number.isSafeInteger(source[key]) && source[key] >= 0);
}
function counts(value: unknown): value is ContinuousCleanupCounts {
  return counters(value, ["studies", "generations", "profiles", "checkpoints", "notificationJobs", "processesReset"]);
}
function retained(value: unknown): value is ContinuousCleanupRetained {
  return counters(value, ["runs", "batches", "qualityIntervals", "analyticsResults", "liveSetups", "configuredProcesses", "configuredSlots"]);
}
function preview(value: unknown): value is ContinuousCleanupPreview {
  const source = record(value);
  return source !== null && typeof source.eligible === "boolean" && typeof source.fingerprint === "string"
    && /^[a-f\d]{32}$/i.test(source.fingerprint) && Array.isArray(source.blockers) && source.blockers.length <= 16
    && source.blockers.every((blocker: unknown) => typeof blocker === "string" && blocker.length <= 120)
    && counts(source.counts) && retained(source.retained);
}
async function request(signal: AbortSignal, fingerprint?: string): Promise<unknown> {
  const response = await apiFetch("/v1/continuous-studies/cleanup", { signal, method: fingerprint === undefined ? "GET" : "POST",
    ...(fingerprint === undefined ? {} : { headers: { "content-type": "application/json" }, body: JSON.stringify({ confirm: true, fingerprint }) }) });
  const value: unknown = await response.json().catch(() => null);
  if (response.ok) return value;
  const detail = record(value);
  const next = preview(detail?.preview) ? detail.preview : undefined;
  const message = response.status === 401 ? "Сессия истекла. Войдите заново перед очисткой."
    : response.status === 403 ? "Недостаточно прав для очистки истории."
    : response.status === 409 && detail?.error === "continuous_cleanup_preview_stale" ? "Состав очистки изменился. Проверьте новые количества и подтвердите заново."
    : response.status === 409 ? "Очистка заблокирована. Сначала остановите непрерывные процессы и дождитесь завершения остановки и доставки."
    : response.status === 400 ? "Подтверждение очистки отклонено. Обновите предварительный просмотр перед повтором."
    : [404, 405, 501].includes(response.status) ? "Очистка истории непрерывных исследований пока недоступна на сервере."
    : `Не удалось очистить историю: HTTP ${response.status}. Обновите предварительный просмотр перед повтором.`;
  throw new ContinuousCleanupApiError(message, response.status, next);
}
export async function previewContinuousCleanup(signal: AbortSignal): Promise<ContinuousCleanupPreview> {
  const value = await request(signal);
  if (!preview(value)) throw new ContinuousCleanupApiError("Сервер вернул неподдерживаемый предварительный просмотр. Очистка недоступна.");
  return value;
}
export async function deleteContinuousHistory(fingerprint: string, signal: AbortSignal): Promise<ContinuousCleanupResult> {
  if (!/^[a-f\d]{32}$/i.test(fingerprint)) throw new ContinuousCleanupApiError("Обновите предварительный просмотр перед очисткой.");
  const value = record(await request(signal, fingerprint));
  if (!value || !counts(value.deleted) || !retained(value.retained)) throw new ContinuousCleanupApiError("Результат очистки не подтверждён. Обновите предварительный просмотр перед повтором.");
  return { deleted: value.deleted, retained: value.retained };
}
