import { apiFetch } from "./platform-api";

export interface TelegramRelaySettings { enabled: boolean; endpointUrl: string; revision: number }
export interface TelegramRelayReason { code: string; message: string }
export interface TelegramRelayStatus {
  endpointUrl: string; settingsRevision: number; secretConfigured: boolean;
  state: "disabled" | "not_configured" | "checking" | "connected" | "error" | "unknown";
  telegramChannel: "unknown" | "ready" | "error";
  checkedAt: string | null; reason: TelegramRelayReason | null;
}
export interface TelegramRelayEnvelope { settings: TelegramRelaySettings; status: TelegramRelayStatus }
export interface TelegramRelayTestResult { requestId: string; state: "sent" | "unknown" | "error"; reason: TelegramRelayReason | null }
export class TelegramRelayApiError extends Error {
  constructor(message: string, readonly status?: number) { super(message); }
}

/** Syntax only; the backend verifies public DNS/IP, certificate and connection pinning. */
export function normalizeTelegramRelayEndpoint(value: string): string {
  const input = value.trim();
  if (!input) return "";
  try {
    const url = new URL(input);
    if (input.length > 2048 || !/^https:\/\/[^/?#]+\/?$/i.test(input) || /[\s\\]/.test(input) || url.protocol !== "https:"
      || url.username || url.password || input.includes("@") || !url.hostname || url.pathname !== "/") throw new Error("invalid origin");
    return `${url.origin}/`;
  } catch {
    throw new TelegramRelayApiError("Укажите HTTPS-адрес relay без пути, логина, пароля, параметров запроса или фрагмента.");
  }
}

const record = (value: unknown): Record<string, unknown> | null => value !== null && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : null;
const revision = (value: unknown): value is number => typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
const text = (value: unknown, limit: number): value is string => typeof value === "string" && value.length <= limit;
function endpoint(value: unknown): value is string {
  if (!text(value, 2048)) return false;
  try { return normalizeTelegramRelayEndpoint(value) === value; } catch { return false; }
}
function reason(value: unknown): value is TelegramRelayReason | null {
  const item = record(value);
  return value === null || (item !== null && text(item.code, 80) && item.code.length > 0 && text(item.message, 512));
}
function checkedAt(value: unknown): value is string | null {
  return value === null || (typeof value === "string" && /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$/.test(value) && Number.isFinite(Date.parse(value)));
}
const invalidResponse = () => new TelegramRelayApiError("Сервер вернул неподдерживаемые данные Telegram relay. Статус не подтверждён.");
function parseEnvelope(value: unknown): TelegramRelayEnvelope {
  const settings = record(record(value)?.settings);
  const status = record(record(value)?.status);
  if (!settings || !status || typeof settings.enabled !== "boolean" || !endpoint(settings.endpointUrl) || !revision(settings.revision)
    || (settings.enabled && !settings.endpointUrl) || !endpoint(status.endpointUrl) || !revision(status.settingsRevision)
    || typeof status.secretConfigured !== "boolean" || typeof status.state !== "string"
    || !["disabled", "not_configured", "checking", "connected", "error", "unknown"].includes(status.state)
    || typeof status.telegramChannel !== "string" || !["unknown", "ready", "error"].includes(status.telegramChannel)
    || !checkedAt(status.checkedAt) || !reason(status.reason)) throw invalidResponse();
  // Project only the public DTO; never retain additional credential/configuration fields.
  return {
    settings: { enabled: settings.enabled, endpointUrl: settings.endpointUrl, revision: settings.revision },
    status: { endpointUrl: status.endpointUrl, settingsRevision: status.settingsRevision, secretConfigured: status.secretConfigured,
      state: status.state as TelegramRelayStatus["state"], telegramChannel: status.telegramChannel as TelegramRelayStatus["telegramChannel"],
      checkedAt: status.checkedAt, reason: status.reason ? { code: status.reason.code, message: status.reason.message } : null },
  };
}

async function request(signal: AbortSignal, method = "GET", suffix = "", body?: unknown): Promise<unknown> {
  const controller = new AbortController();
  let timedOut = false;
  const abort = () => { controller.abort(); };
  let rejectAbort: (() => void) | undefined;
  const aborted = new Promise<never>((_resolve, reject) => {
    rejectAbort = () => { reject(new DOMException("Request aborted", "AbortError")); };
    controller.signal.addEventListener("abort", rejectAbort, { once: true });
  });
  signal.addEventListener("abort", abort, { once: true });
  if (signal.aborted) abort();
  const timer = setTimeout(() => { timedOut = true; controller.abort(); }, 10000);
  async function perform(): Promise<unknown> {
    controller.signal.throwIfAborted();
    const response = await apiFetch(`/v1/telegram-relay${suffix}`, { method, signal: controller.signal,
      ...(body === undefined ? {} : { headers: { "content-type": "application/json" }, body: JSON.stringify(body) }) });
    controller.signal.throwIfAborted();
    if (!response.ok) {
      // Inspect only the allowlisted conflict code; never surface raw remote diagnostics.
      const detail = response.status === 409 ? record(await response.json().catch(() => null)) : null;
      const endpointBusy = detail?.error === "telegram_relay_endpoint_busy" || detail?.code === "telegram_relay_endpoint_busy";
      const message = response.status === 401 ? "Сессия истекла. Войдите заново для настройки Telegram."
        : response.status === 403 ? "Недостаточно прав для настройки Telegram relay."
        : response.status === 409 && endpointBusy ? "Дождитесь завершения текущих отправок и повторите сохранение. Черновик адреса сохранён."
        : response.status === 409 ? "Настройки relay изменились. Черновик сохранён; сбросьте изменения, чтобы загрузить актуальную ревизию."
        : [404, 405, 501].includes(response.status) ? "Telegram relay не поддерживается этой версией сервера."
        : response.status === 400 ? "Сервер отклонил настройки или команду relay. Проверьте HTTPS-адрес и публичность сервера."
        : `Telegram relay: ошибка платформы HTTP ${response.status}.`;
      throw new TelegramRelayApiError(message, response.status);
    }
    return response.json().catch(() => { throw invalidResponse(); });
  }
  try { return await Promise.race([perform(), aborted]); }
  catch (cause) {
    if (signal.aborted) throw new DOMException("Request aborted", "AbortError");
    if (timedOut) throw new TelegramRelayApiError("Платформа не ответила за 10 секунд. Результат команды не подтверждён.");
    if (cause instanceof TelegramRelayApiError) throw cause;
    throw new TelegramRelayApiError("Не удалось связаться с платформой. Статус Telegram relay не подтверждён.");
  } finally {
    clearTimeout(timer); signal.removeEventListener("abort", abort);
    if (rejectAbort) controller.signal.removeEventListener("abort", rejectAbort);
  }
}

export async function getTelegramRelay(signal: AbortSignal): Promise<TelegramRelayEnvelope> {
  return parseEnvelope(await request(signal));
}
export async function saveTelegramRelay(settings: TelegramRelaySettings, signal: AbortSignal): Promise<TelegramRelayEnvelope> {
  const endpointUrl = normalizeTelegramRelayEndpoint(settings.endpointUrl);
  if (!revision(settings.revision) || (settings.enabled && !endpointUrl)) throw new TelegramRelayApiError("Для включения Telegram укажите HTTPS-адрес и загрузите актуальные настройки.");
  return parseEnvelope(await request(signal, "PUT", "", { enabled: settings.enabled, endpointUrl, expectedRevision: settings.revision }));
}
export async function probeTelegramRelay(expectedRevision: number, signal: AbortSignal): Promise<TelegramRelayEnvelope> {
  if (!revision(expectedRevision)) throw new TelegramRelayApiError("Загрузите актуальную ревизию настроек relay.");
  return parseEnvelope(await request(signal, "POST", "/probe", { expectedRevision }));
}
export async function testTelegramRelay(expectedRevision: number, requestId: string, signal: AbortSignal): Promise<TelegramRelayTestResult> {
  if (!revision(expectedRevision) || !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(requestId)) throw new TelegramRelayApiError("Не удалось создать безопасный идентификатор теста. Обновите браузер.");
  const value = record(await request(signal, "POST", "/test", { expectedRevision, requestId }));
  if (value?.requestId !== requestId || typeof value.state !== "string" || !["sent", "unknown", "error"].includes(value.state) || !reason(value.reason)) throw invalidResponse();
  return { requestId, state: value.state as TelegramRelayTestResult["state"], reason: value.reason ? { code: value.reason.code, message: value.reason.message } : null };
}
