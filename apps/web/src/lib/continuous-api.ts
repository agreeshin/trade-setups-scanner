import { apiFetch } from "./platform-api";
import type { OptimizationSpec } from "./optimization-api";

// The server fixes dates and maturity at actual execution. Never manufacture them in a browser loop.
export type ContinuousStudySpec = Omit<OptimizationSpec, "fromUnix" | "toUnix" | "minSetupsPerDay" | "setupMaturity">;
export const MAX_CONTINUOUS_THRESHOLDS = 64;
export const MAX_CONTINUOUS_LOOKBACK_DAYS = 3660;
export const MIN_CONTINUOUS_LOOKBACK_DAYS = 60 / 86400;
export function isContinuousLookbackDays(days: number): boolean {
  const seconds = days * 86400;
  return Number.isFinite(days) && seconds >= 60 && seconds <= MAX_CONTINUOUS_LOOKBACK_DAYS * 86400 && Number.isInteger(seconds);
}
export interface ContinuousConfig {
  lookbackDays: number;
  minSetupsPerDay: number[];
  minMeanExpectancy: number;
  adaptiveEnabled: boolean;
  study: ContinuousStudySpec;
}
export interface RecommendedCell {
  tpPct: number;
  slPct: number;
  rawR: number;
  meanR: number;
  robustScore: number;
}
export interface ContinuousSlot {
  id: number;
  minSetupsPerDay: number;
  status: string;
  activeGenerationId: number | null;
  // Optional only for rollout compatibility; absence is unknown, never implicitly ready.
  activeStatus?: "active" | "inactive";
  pendingGenerationId?: number | null;
  pendingStatus?: string | null;
  /** Unix seconds at the CLOSED minute boundary, not candle-open time. */
  readinessFrontier?: number | null;
  /** Opaque serde_json::Value from response.json(); do not assume an error-object schema. */
  pendingError?: unknown;
  liveWatermark?: number | null;
  studyId: number | null;
  meanExpectancy: number | null;
  selectedCell: RecommendedCell | null;
  updatedAt: string;
}
export interface ContinuousProcess {
  // The unconfigured detail response has no persisted owner yet.
  ownerId?: string;
  algorithmId: string;
  status: "stopped" | "running" | "stopping" | "error";
  configRevision: number;
  config: ContinuousConfig | null;
  activeStudyId: number | null;
  phase: string | null;
  error: { code: string; message: string } | null;
  slots: ContinuousSlot[];
}

export class ContinuousApiError extends Error {
  constructor(message: string, readonly status?: number) { super(message); }
}

function isProcess(value: unknown): value is ContinuousProcess {
  if (value === null || typeof value !== "object") return false;
  const process = value as Partial<ContinuousProcess>;
  const nullableId = (id: unknown) => id === null || (typeof id === "number" && Number.isSafeInteger(id) && id > 0);
  const optionalClosedBoundary = (time: unknown) => time === undefined || time === null || (typeof time === "number"
    && Number.isSafeInteger(time) && time >= 0 && time % 60 === 0 && Number.isFinite(new Date(time * 1000).getTime()));
  const config = process.config;
  const validConfig = config === null || (config !== undefined && typeof config === "object" && typeof config.adaptiveEnabled === "boolean"
    && isContinuousLookbackDays(config.lookbackDays) && Number.isFinite(config.minMeanExpectancy)
    && Array.isArray(config.minSetupsPerDay) && config.minSetupsPerDay.length > 0 && config.minSetupsPerDay.length <= MAX_CONTINUOUS_THRESHOLDS
    && new Set(config.minSetupsPerDay).size === config.minSetupsPerDay.length && config.minSetupsPerDay.every((threshold) => typeof threshold === "number" && Number.isFinite(threshold) && threshold >= 0)
    && config.study !== null && typeof config.study === "object" && config.study.algorithmId === process.algorithmId
    && Array.isArray(config.study.instruments) && (config.study.direction === "short" || config.study.direction === "long"));
  return typeof process.algorithmId === "string" && ["stopped", "running", "stopping", "error"].includes(process.status ?? "")
    && Number.isSafeInteger(process.configRevision) && validConfig && nullableId(process.activeStudyId)
    && (process.phase === null || typeof process.phase === "string")
    && (process.error === null || (process.error !== undefined && typeof process.error === "object" && typeof process.error.code === "string" && typeof process.error.message === "string"))
    && Array.isArray(process.slots) && process.slots.every((slot) => slot !== null && typeof slot === "object"
      && Number.isSafeInteger(slot.id) && slot.id > 0 && typeof slot.status === "string" && Number.isFinite(slot.minSetupsPerDay) && nullableId(slot.activeGenerationId) && nullableId(slot.studyId)
      && (slot.activeStatus === undefined || slot.activeStatus === "active" || slot.activeStatus === "inactive")
      && (slot.pendingGenerationId === undefined || nullableId(slot.pendingGenerationId))
      && (slot.pendingStatus === undefined || slot.pendingStatus === null || typeof slot.pendingStatus === "string")
      && optionalClosedBoundary(slot.readinessFrontier) && optionalClosedBoundary(slot.liveWatermark)
      && (slot.meanExpectancy === null || Number.isFinite(slot.meanExpectancy))
      && typeof slot.updatedAt === "string" && Number.isFinite(Date.parse(slot.updatedAt))
      && (slot.selectedCell === null || (typeof slot.selectedCell === "object" && slot.selectedCell !== undefined && [slot.selectedCell.tpPct, slot.selectedCell.slPct, slot.selectedCell.rawR, slot.selectedCell.meanR, slot.selectedCell.robustScore].every(Number.isFinite))));
}

async function request(path: string, signal: AbortSignal, method = "GET", config?: ContinuousConfig): Promise<unknown> {
  const response = await apiFetch(path, { method, signal, ...(config ? { headers: { "content-type": "application/json" }, body: JSON.stringify(config) } : {}) });
  if (!response.ok) {
    if (response.status === 404 || response.status === 501) throw new ContinuousApiError("Непрерывные исследования пока недоступны на сервере. Ручная оптимизация и постоянный live остаются доступны.", response.status);
    const detail = await response.json().catch(() => null) as { message?: unknown; error?: unknown } | null;
    const reason = typeof detail?.message === "string" ? detail.message.slice(0, 300) : null;
    throw new ContinuousApiError(`${response.status === 409 ? "Состояние процесса изменилось. Обновите статус перед повтором." : `Не удалось выполнить команду непрерывного исследования: HTTP ${response.status}.`}${reason ? ` ${reason}` : ""}`, response.status);
  }
  return response.json() as Promise<unknown>;
}

const processPath = (algorithmId: string) => `/v1/continuous-studies/${encodeURIComponent(algorithmId)}`;
async function processRequest(algorithmId: string, signal: AbortSignal, method = "GET", config?: ContinuousConfig, action = ""): Promise<ContinuousProcess> {
  const value = await request(`${processPath(algorithmId)}${action}`, signal, method, config);
  if (!isProcess(value) || value.algorithmId !== algorithmId) throw new ContinuousApiError("Сервер вернул неподдерживаемый статус непрерывного исследования. Управление недоступно до обновления статуса.");
  return value;
}
export async function listContinuousStudies(algorithmId: string, signal: AbortSignal): Promise<ContinuousProcess[]> {
  const value = await request(`/v1/continuous-studies?${new URLSearchParams({ algorithmId })}`, signal);
  if (value === null || typeof value !== "object" || !("processes" in value) || !Array.isArray(value.processes) || !value.processes.every(isProcess)) {
    throw new ContinuousApiError("Сервер не поддерживает контракт непрерывных исследований. Ручная оптимизация и постоянный live остаются доступны.");
  }
  return value.processes.filter((process) => process.algorithmId === algorithmId);
}
export const getContinuousStudy = (algorithmId: string, signal: AbortSignal): Promise<ContinuousProcess> => processRequest(algorithmId, signal);
export const saveContinuousStudy = (algorithmId: string, config: ContinuousConfig, signal: AbortSignal): Promise<ContinuousProcess> => processRequest(algorithmId, signal, "PUT", config);
export const startContinuousStudy = (algorithmId: string, signal: AbortSignal): Promise<ContinuousProcess> => processRequest(algorithmId, signal, "POST", undefined, "/start");
export const stopContinuousStudy = (algorithmId: string, signal: AbortSignal): Promise<ContinuousProcess> => processRequest(algorithmId, signal, "POST", undefined, "/stop");
