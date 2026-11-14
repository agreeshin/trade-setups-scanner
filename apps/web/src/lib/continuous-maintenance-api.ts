import { apiFetch } from "./platform-api";

export interface ContinuousMaintenanceStatus {
  admissionPaused: boolean;
  state: "running" | "draining" | "drained";
  activeChains: number;
  /** Count of committed frozen manifests, not a checkpoint compatibility guarantee. */
  resumableChains: number;
  updatedAt: string | null;
}
export type ContinuousMaintenanceAction = "drain" | "resume";
export class ContinuousMaintenanceApiError extends Error {
  constructor(message: string, readonly status?: number) { super(message); }
}

function isStatus(value: unknown): value is ContinuousMaintenanceStatus {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return false;
  const status = value as ContinuousMaintenanceStatus;
  const count = (number: unknown) => typeof number === "number" && Number.isSafeInteger(number) && number >= 0;
  const timestamp = status.updatedAt === null || (typeof status.updatedAt === "string"
    && /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$/.test(status.updatedAt) && Number.isFinite(Date.parse(status.updatedAt)));
  return typeof status.admissionPaused === "boolean" && count(status.activeChains) && count(status.resumableChains) && timestamp
    && (status.admissionPaused ? status.state === (status.activeChains > 0 ? "draining" : "drained") : status.state === "running");
}

async function request(signal: AbortSignal, action?: ContinuousMaintenanceAction): Promise<ContinuousMaintenanceStatus> {
  const response = await apiFetch(`/v1/continuous-studies/maintenance${action ? `/${action}` : ""}`, { signal, method: action ? "POST" : "GET" });
  if (!response.ok) {
    const message = response.status === 401 ? "Сессия истекла. Войдите заново перед обслуживанием."
      : response.status === 403 ? "Недостаточно прав для обслуживания непрерывных исследований."
      : [404, 405, 501].includes(response.status) ? "Обслуживание непрерывных исследований не поддерживается этой версией сервера."
      : response.status === 409 ? "Состояние обслуживания изменилось. Обновите статус перед повтором."
      : `Не удалось ${action ? "изменить" : "получить"} статус обслуживания: HTTP ${response.status}.`;
    throw new ContinuousMaintenanceApiError(message, response.status);
  }
  const value: unknown = await response.json().catch(() => null);
  if (!isStatus(value)) throw new ContinuousMaintenanceApiError("Сервер вернул неподдерживаемый статус обслуживания. Управление недоступно до успешного обновления.");
  return value;
}

export const getContinuousMaintenance = (signal: AbortSignal): Promise<ContinuousMaintenanceStatus> => request(signal);
export const drainContinuousMaintenance = (signal: AbortSignal): Promise<ContinuousMaintenanceStatus> => request(signal, "drain");
export const resumeContinuousMaintenance = (signal: AbortSignal): Promise<ContinuousMaintenanceStatus> => request(signal, "resume");
