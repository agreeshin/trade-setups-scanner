import { apiFetch } from "./platform-api";
import type { ContinuousSlot } from "./continuous-api";

/** Live identity and control, never inferred from research queue membership. */
export interface AdaptiveProfile extends ContinuousSlot {
  algorithmId: string;
  desiredRunning: boolean;
  controlRevision: number;
  acceptedGenerationId: number;
  direction: "long" | "short" | null;
  adaptiveEnabled: boolean;
  blockingReason: string | null;
  error: unknown;
  pendingAttempts: number;
  attemptLimit: number;
  initializationStatus: string | null;
  inResearchQueue: boolean;
}
export interface ProfilePage {
  profiles: AdaptiveProfile[];
  total: number; offset: number; limit: number;
  limits: { totalProfiles: number; runningProfiles: number };
  blockedHandoffs: { generationId: number; slotId: number; error: unknown }[];
}
export interface ProfileCommand {
  expectedRevision: number; expectedGenerationId: number; requestId: string;
}
export interface ProfileReceipt {
  profileId: number; action: "stop" | "play"; desiredRunning: boolean;
  controlRevision: number; acceptedGenerationId: number; requestId: string;
  grantedAttempts: number; attemptLimit: number; status: string;
}
export class AdaptiveProfileApiError extends Error {
  constructor(message: string, readonly status?: number) { super(message); }
}
const positiveId = (value: unknown): value is number => typeof value === "number" && Number.isSafeInteger(value) && value > 0;
const nullableId = (value: unknown) => value === null || positiveId(value);
function isProfile(value: unknown): value is AdaptiveProfile {
  if (value === null || typeof value !== "object") return false;
  const p = value as AdaptiveProfile;
  const cell = p.selectedCell;
  return positiveId(p.id) && typeof p.algorithmId === "string" && Number.isFinite(p.minSetupsPerDay) && p.minSetupsPerDay >= 0
    && typeof p.desiredRunning === "boolean" && positiveId(p.controlRevision) && positiveId(p.acceptedGenerationId)
    && nullableId(p.activeGenerationId) && nullableId(p.pendingGenerationId) && nullableId(p.studyId)
    && typeof p.status === "string" && ["active", "inactive", "error"].includes(p.activeStatus ?? "")
    && typeof p.adaptiveEnabled === "boolean" && typeof p.inResearchQueue === "boolean"
    && (p.direction === null || p.direction === "long" || p.direction === "short")
    && (p.blockingReason === null || typeof p.blockingReason === "string")
    && Number.isSafeInteger(p.pendingAttempts) && p.pendingAttempts >= 0 && positiveId(p.attemptLimit)
    && (p.initializationStatus === null || typeof p.initializationStatus === "string")
    && (p.meanExpectancy === null || Number.isFinite(p.meanExpectancy))
    && (p.trendMode == null || ["mean", "forecast", "blend"].includes(p.trendMode))
    && (p.trendBlendLambda == null || (Number.isFinite(p.trendBlendLambda) && p.trendBlendLambda > 0 && p.trendBlendLambda < 1))
    && (cell === null || (typeof cell === "object" && [cell.tpPct, cell.slPct, cell.rawR, cell.meanR, cell.robustScore].every(Number.isFinite)))
    && (cell === null || (cell.forecastR === undefined && cell.forecastMatrixR === undefined) || [cell.forecastR, cell.forecastMatrixR].every(Number.isFinite))
    && (p.liveReadinessSummary == null || (typeof p.liveReadinessSummary === "object" && Array.isArray(p.liveReadinessSummary.waitingReasons)
      && p.liveReadinessSummary.waitingReasons.length <= 8 && p.liveReadinessSummary.waitingReasons.every((entry) => entry !== null && typeof entry === "object" && typeof entry.reason === "string" && typeof entry.count === "number")))
    && typeof p.updatedAt === "string" && Number.isFinite(Date.parse(p.updatedAt));
}
async function request(path: string, signal: AbortSignal, method = "GET", body?: unknown): Promise<unknown> {
  const response = await apiFetch(path, { method, signal, ...(body === undefined ? {} : { headers: { "content-type": "application/json" }, body: JSON.stringify(body) }) });
  if (!response.ok) {
    if ([404, 501].includes(response.status)) throw new AdaptiveProfileApiError("Самостоятельные адаптивные профили недоступны на сервере. Общее управление исследованием не используется вместо Stop/Play профиля.", response.status);
    const detail = await response.json().catch(() => null) as { error?: unknown; limit?: unknown } | null;
    const code = typeof detail?.error === "string" ? detail.error.slice(0, 120) : "";
    const limit = typeof detail?.limit === "number" ? ` Лимит: ${detail.limit}.` : "";
    throw new AdaptiveProfileApiError(`${response.status === 409 ? "Состояние профиля изменилось; обновите статус." : response.status === 429 ? "Достигнута квота профилей; действующие профили сохранены." : `Команда профиля не выполнена: HTTP ${response.status}.`}${limit}${code ? ` ${code}` : ""}`, response.status);
  }
  return response.json() as Promise<unknown>;
}
export async function listAdaptiveProfiles(algorithmId: string, offset: number, signal: AbortSignal): Promise<ProfilePage> {
  const value = await request(`/v1/adaptive-profiles?${new URLSearchParams({ algorithmId, offset: String(offset), limit: "50" })}`, signal) as ProfilePage | null;
  if (!value || !Array.isArray(value.profiles) || value.profiles.length > 50 || !value.profiles.every((p) => isProfile(p) && p.algorithmId === algorithmId)
    || !Number.isSafeInteger(value.total) || value.total < value.profiles.length || value.offset !== offset || value.limit !== 50
    || !value.limits || !positiveId(value.limits.totalProfiles) || !positiveId(value.limits.runningProfiles) || value.limits.runningProfiles > value.limits.totalProfiles
    || !Array.isArray(value.blockedHandoffs) || value.blockedHandoffs.length > 50
    || !value.blockedHandoffs.every((h) => h !== null && typeof h === "object" && positiveId(h.generationId) && positiveId(h.slotId))) {
    throw new AdaptiveProfileApiError("Сервер вернул неподдерживаемый список самостоятельных профилей. Управление недоступно до обновления статуса.");
  }
  return value;
}
export async function commandAdaptiveProfile(profile: AdaptiveProfile, action: "stop" | "play", command: ProfileCommand, signal: AbortSignal): Promise<ProfileReceipt> {
  const value = await request(`/v1/adaptive-profiles/${profile.id}/${action}`, signal, "POST", command) as ProfileReceipt | null;
  if (value?.profileId !== profile.id || value.action !== action || value.requestId !== command.requestId
    || value.acceptedGenerationId !== command.expectedGenerationId || !positiveId(value.controlRevision)
    || value.desiredRunning !== (action === "play") || ![0, 3].includes(value.grantedAttempts) || !positiveId(value.attemptLimit) || typeof value.status !== "string") {
    throw new AdaptiveProfileApiError("Подтверждение команды профиля не распознано. Обновите статус; повтор использует тот же ключ запроса.");
  }
  return value;
}
export async function adaptivePermission(algorithmId: string, signal: AbortSignal, enabled?: boolean): Promise<boolean> {
  const value = await request(`/v1/adaptive-live/${encodeURIComponent(algorithmId)}/permission`, signal, enabled === undefined ? "GET" : "PUT", enabled === undefined ? undefined : { enabled }) as { algorithmId?: unknown; enabled?: unknown } | null;
  if (value?.algorithmId !== algorithmId || typeof value.enabled !== "boolean") throw new AdaptiveProfileApiError("Разрешение адаптивного live не подтверждено сервером.");
  return value.enabled;
}
