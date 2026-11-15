import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { TelegramRelayEnvelope, TelegramRelayTestResult } from "../lib/telegram-relay-api";
import { TelegramRelaySettingsSection } from "./telegram-relay-settings";
import { RouteActiveContext } from "./route-active";

const path = "/v1/telegram-relay";
const requestId = "d380c45e-89aa-49c8-b6d4-7e4d1c0ead15";
const uuid = vi.fn((): ReturnType<Crypto["randomUUID"]> => requestId);
const configured: TelegramRelayEnvelope = {
  settings: { enabled: true, endpointUrl: "https://relay.example/", revision: 2 },
  status: { endpointUrl: "https://relay.example/", settingsRevision: 2, secretConfigured: true, state: "connected", telegramChannel: "unknown", checkedAt: "2026-11-14T12:00:00Z", reason: null },
};
let server: TelegramRelayEnvelope;
let readStatus: number;
let outcome: TelegramRelayTestResult["state"];
let endpointBusy: boolean;
const calls: { path: string; method: string; body: unknown; signal: AbortSignal | null | undefined }[] = [];
const json = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status });
const requestPath = (url: RequestInfo | URL) => typeof url === "string" ? url : url instanceof URL ? url.href : url.url;
function record(url: RequestInfo | URL, init?: RequestInit) {
  const call = { path: requestPath(url), method: init?.method ?? "GET", body: typeof init?.body === "string" ? JSON.parse(init.body) as unknown : null, signal: init?.signal };
  calls.push(call); return call;
}
function fetchRelay(url: RequestInfo | URL, init?: RequestInit): Promise<Response> {
  const call = record(url, init);
  if (call.method === "GET") return Promise.resolve(json(server, readStatus));
  const input = call.body as { enabled?: boolean; endpointUrl?: string; expectedRevision: number; requestId?: string };
  if (input.expectedRevision !== server.settings.revision) return Promise.resolve(json({}, 409));
  if (call.method === "PUT") {
    if (endpointBusy) return Promise.resolve(json({ error: "telegram_relay_endpoint_busy", message: "PRIVATE_RAW_DIAGNOSTIC" }, 409));
    const revision = server.settings.revision + 1;
    server = { settings: { enabled: input.enabled!, endpointUrl: input.endpointUrl!, revision },
      status: { ...server.status, endpointUrl: input.endpointUrl!, settingsRevision: revision, state: input.enabled ? "unknown" : "disabled", checkedAt: null, telegramChannel: "unknown", reason: null } };
    return Promise.resolve(json(server));
  }
  if (call.path.endsWith("/probe")) {
    server = { ...server, status: { ...server.status, state: "connected", checkedAt: "2026-11-14T12:01:00Z" } };
    return Promise.resolve(json(server));
  }
  return Promise.resolve(json({ requestId: input.requestId, state: outcome, reason: null }));
}
function delayNext() {
  let finish: ((response: Response) => void) | undefined;
  vi.mocked(globalThis.fetch).mockImplementationOnce((url, init) => { record(url, init); return new Promise<Response>((resolve) => { finish = resolve; }); });
  return async (value: unknown) => { await act(async () => { finish?.(json(value)); await Promise.resolve(); }); };
}
const tick = async (ms = 0) => { await act(async () => { await vi.advanceTimersByTimeAsync(ms); }); };
const mutations = () => calls.filter((call) => call.method !== "GET");
const gets = () => calls.filter((call) => call.method === "GET");
const endpoint = () => screen.getByRole("textbox", { name: "HTTPS-адрес Telegram relay" });
const save = () => screen.getByRole("button", { name: "Сохранить Telegram relay" });
const probe = () => screen.getByRole("button", { name: "Проверить Telegram relay" });
const test = () => screen.getByRole("button", { name: "Отправить тест в Telegram" });
const refresh = () => screen.getByRole("button", { name: "Обновить статус Telegram" });
beforeEach(() => {
  vi.useFakeTimers(); calls.length = 0; server = structuredClone(configured); readStatus = 200; outcome = "sent"; endpointBusy = false;
  vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
  uuid.mockClear(); vi.spyOn(crypto, "randomUUID").mockImplementation(uuid);
  vi.stubGlobal("fetch", vi.fn<typeof globalThis.fetch>(fetchRelay));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("keeps relay authentication separate from channel readiness and never probes or sends on render/read/poll", async () => {
  render(<TelegramRelaySettingsSection active />); await tick();
  expect(screen.getByText("Relay доступен · ключ подтверждён")).toBeInTheDocument();
  expect(screen.getByText("Telegram-канал: готовность неизвестна.")).toBeInTheDocument();
  expect(screen.getByText(/Ключ подписи на основном сервере: настроен/)).toBeInTheDocument();
  await tick(14999); expect(gets()).toHaveLength(1);
  await tick(1); expect(gets()).toHaveLength(2); expect(mutations()).toHaveLength(0);
  expect(uuid).not.toHaveBeenCalled();
});

it("saves a disabled empty setup or canonical HTTPS origin without touching age/Push/live or restarting services", async () => {
  server = { settings: { enabled: false, endpointUrl: "", revision: 0 }, status: { ...configured.status, endpointUrl: "", settingsRevision: 0, state: "disabled", checkedAt: null } };
  render(<TelegramRelaySettingsSection active />); await tick();
  expect(screen.getByText("Доставка отключена")).toBeInTheDocument(); expect(probe()).toBeDisabled(); expect(test()).toBeDisabled();
  fireEvent.click(screen.getByRole("checkbox", { name: "Включить Telegram" }));
  expect(save()).toBeDisabled();
  fireEvent.change(endpoint(), { target: { value: "http://relay.example/" } }); expect(save()).toBeDisabled();
  fireEvent.change(endpoint(), { target: { value: "https://NEW.example" } });
  expect(probe()).toBeDisabled(); expect(test()).toBeDisabled(); expect(mutations()).toHaveLength(0);
  fireEvent.click(save()); await tick();
  expect(mutations()).toEqual([expect.objectContaining({ path, method: "PUT", body: { enabled: true, endpointUrl: "https://new.example/", expectedRevision: 0 } })]);
  expect(endpoint()).toHaveValue("https://new.example/"); expect(server.settings.revision).toBe(1);
  expect(screen.getByText(/Настройки Telegram сохранены/)).toHaveTextContent("Перезапуск сервисов не требуется");
  expect(screen.queryByText("Relay доступен · ключ подтверждён")).not.toBeInTheDocument();
  fireEvent.click(probe()); await tick();
  expect(mutations()[1]).toMatchObject({ path: `${path}/probe`, method: "POST", body: { expectedRevision: 1 } });
  expect(calls.every((call) => call.path.startsWith(path))).toBe(true);
});

it("sends one explicit revision-bound test with one stable randomUUID and claims sent only for its confirmed result", async () => {
  render(<TelegramRelaySettingsSection active />); await tick();
  fireEvent.click(test()); fireEvent.click(test()); await tick();
  expect(mutations()).toEqual([expect.objectContaining({ path: `${path}/test`, body: { expectedRevision: 2, requestId } })]);
  expect(uuid).toHaveBeenCalledOnce();
  expect(screen.getByText(/Тестовое сообщение отправлено: Telegram подтвердил отправку/)).toHaveTextContent("не подтверждает прочтение");
  await tick(30000); expect(mutations()).toHaveLength(1); expect(uuid).toHaveBeenCalledOnce();
});
it.each(["unknown", "error"] as const)("shows explicit test outcome %s without manufacturing a sent acknowledgment or retry", async (state) => {
  outcome = state;
  render(<TelegramRelaySettingsSection active />); await tick();
  fireEvent.click(test()); await tick();
  expect(screen.getByRole("alert")).toHaveTextContent(state === "unknown" ? "Результат теста неизвестен" : "Тест завершился ошибкой");
  expect(screen.queryByText(/Тестовое сообщение отправлено:/)).not.toBeInTheDocument();
  await tick(30000); expect(mutations()).toHaveLength(1);
});

it("times out a test at 10s as unknown, never retries it and ignores a late success", async () => {
  render(<TelegramRelaySettingsSection active />); await tick();
  const finish = delayNext(); fireEvent.click(test()); await tick();
  expect(save()).toBeDisabled(); expect(probe()).toBeDisabled(); expect(test()).toBeDisabled();
  await tick(10000);
  expect(screen.getByText(/Результат теста неизвестен:/)).toHaveTextContent("сообщение могло быть отправлено");
  expect(mutations()).toHaveLength(1); expect(mutations()[0]?.signal?.aborted).toBe(true);
  await finish({ requestId, state: "sent", reason: null }); await tick(30000);
  expect(screen.queryByText(/Тестовое сообщение отправлено:/)).not.toBeInTheDocument();
  expect(mutations()).toHaveLength(1); expect(uuid).toHaveBeenCalledOnce();
});

it("retains a dirty address across a slow poll and never revives old-endpoint green or enables unsaved commands", async () => {
  render(<TelegramRelaySettingsSection active />); await tick();
  const finish = delayNext(); await tick(15000);
  fireEvent.change(endpoint(), { target: { value: "https://new.example/" } });
  await finish(configured); await tick();
  expect(endpoint()).toHaveValue("https://new.example/");
  expect(screen.getByText(/Сначала сохраните изменения/)).toBeInTheDocument();
  expect(screen.queryByText("Relay доступен · ключ подтверждён")).not.toBeInTheDocument();
  expect(probe()).toBeDisabled(); expect(test()).toBeDisabled(); expect(save()).toBeEnabled();
  expect(mutations()).toHaveLength(0);
});
it.each(["endpoint", "revision"])("does not trust connected status bound to a different %s", async (mismatch) => {
  server.status = { ...server.status, telegramChannel: "ready", ...(mismatch === "endpoint" ? { endpointUrl: "https://old.example/" } : { settingsRevision: 1 }) };
  render(<TelegramRelaySettingsSection active />); await tick();
  expect(screen.queryByText("Relay доступен · ключ подтверждён")).not.toBeInTheDocument();
  expect(screen.getByText("Telegram-канал: готовность неизвестна.")).toBeInTheDocument();
  expect(probe()).toBeDisabled(); expect(test()).toBeDisabled();
});

it("retains the original draft revision on concurrent change/409 and only resets on explicit user action", async () => {
  render(<TelegramRelaySettingsSection active />); await tick();
  fireEvent.change(endpoint(), { target: { value: "https://draft.example/" } });
  server = { settings: { enabled: true, endpointUrl: "https://other.example/", revision: 3 },
    status: { ...server.status, endpointUrl: "https://other.example/", settingsRevision: 3, state: "unknown", checkedAt: null } };
  await tick(15000); expect(endpoint()).toHaveValue("https://draft.example/");
  fireEvent.click(save()); await tick();
  expect(mutations()[0]?.body).toEqual({ enabled: true, endpointUrl: "https://draft.example/", expectedRevision: 2 });
  expect(screen.getByRole("alert")).toHaveTextContent("Черновик сохранён");
  expect(endpoint()).toHaveValue("https://draft.example/");
  await tick(15000); expect(mutations()).toHaveLength(1);
  fireEvent.click(screen.getByRole("button", { name: "Сбросить изменения relay" }));
  expect(endpoint()).toHaveValue("https://other.example/"); expect(screen.queryByRole("alert")).not.toBeInTheDocument();
});

it("keeps the new-origin draft on endpoint-busy 409 and retries saving only on another explicit click", async () => {
  render(<TelegramRelaySettingsSection active />); await tick();
  fireEvent.change(endpoint(), { target: { value: "https://new.example/" } });
  endpointBusy = true; fireEvent.click(save()); await tick();
  expect(screen.getByRole("alert")).toHaveTextContent("Дождитесь завершения текущих отправок и повторите сохранение");
  expect(screen.queryByText(/PRIVATE_RAW_DIAGNOSTIC/)).not.toBeInTheDocument();
  expect(endpoint()).toHaveValue("https://new.example/"); expect(server.settings.endpointUrl).toBe("https://relay.example/");
  await tick(30000); expect(mutations()).toHaveLength(1); expect(probe()).toBeDisabled(); expect(test()).toBeDisabled();
  endpointBusy = false; fireEvent.click(save()); await tick();
  expect(mutations()).toHaveLength(2); expect(server.settings.endpointUrl).toBe("https://new.example/");
  expect(screen.queryByText("Relay доступен · ключ подтверждён")).not.toBeInTheDocument();
  fireEvent.click(probe()); await tick(); expect(screen.getByText("Relay доступен · ключ подтверждён")).toBeInTheDocument();
});

it("pauses all status observation for hidden subsection, route and document, preserving draft and resuming immediately", async () => {
  const visibility = vi.spyOn(document, "visibilityState", "get");
  const view = render(<RouteActiveContext.Provider value><TelegramRelaySettingsSection active={false} /></RouteActiveContext.Provider>);
  await tick(60000); expect(calls).toHaveLength(0);
  view.rerender(<RouteActiveContext.Provider value><TelegramRelaySettingsSection active /></RouteActiveContext.Provider>); await tick();
  fireEvent.change(endpoint(), { target: { value: "https://draft.example/" } });
  view.rerender(<RouteActiveContext.Provider value><TelegramRelaySettingsSection active={false} /></RouteActiveContext.Provider>);
  await tick(60000); expect(gets()).toHaveLength(1); expect(vi.getTimerCount()).toBe(0);
  view.rerender(<RouteActiveContext.Provider value={false}><TelegramRelaySettingsSection active /></RouteActiveContext.Provider>);
  await tick(60000); expect(gets()).toHaveLength(1);
  view.rerender(<RouteActiveContext.Provider value><TelegramRelaySettingsSection active /></RouteActiveContext.Provider>); await tick();
  expect(gets()).toHaveLength(2); expect(endpoint()).toHaveValue("https://draft.example/");
  visibility.mockReturnValue("hidden"); act(() => { document.dispatchEvent(new Event("visibilitychange")); });
  await tick(60000); expect(gets()).toHaveLength(2); expect(vi.getTimerCount()).toBe(0);
  visibility.mockReturnValue("visible"); act(() => { document.dispatchEvent(new Event("visibilitychange")); });
  await tick(); expect(gets()).toHaveLength(3); expect(endpoint()).toHaveValue("https://draft.example/");
  view.unmount(); expect(vi.getTimerCount()).toBe(0);
});

it.each(["5xx", "timeout", "invalid DTO"])("keeps the last snapshot stale/disabled after transient %s and recovers via GET after 15s", async (failure) => {
  render(<TelegramRelaySettingsSection active />); await tick();
  if (failure === "5xx") readStatus = 503;
  else if (failure === "timeout") delayNext();
  else vi.mocked(globalThis.fetch).mockResolvedValueOnce(json({}));
  await tick(15000); if (failure === "timeout") await tick(10000);
  expect(screen.getByText(/Показан последний подтверждённый снимок/)).toBeInTheDocument();
  expect(probe()).toBeDisabled(); expect(test()).toBeDisabled();
  expect(screen.queryByText("Relay доступен · ключ подтверждён")).not.toBeInTheDocument();
  readStatus = 200; const count = gets().length;
  await tick(14999); expect(gets()).toHaveLength(count);
  await tick(1); expect(probe()).toBeEnabled(); expect(gets()).toHaveLength(count + 1);
  expect(mutations()).toHaveLength(0);
});
it.each([401, 403, 404, 405, 501])("suspends automatic reads after HTTP %i until manual refresh", async (status) => {
  readStatus = status; render(<TelegramRelaySettingsSection active />); await tick(60000);
  expect(gets()).toHaveLength(1); expect(mutations()).toHaveLength(0); expect(probe()).toBeDisabled();
  readStatus = 200; fireEvent.click(refresh()); await tick(); expect(probe()).toBeEnabled();
});

it("aborts on subsection cleanup and discards late old responses after a new observer is active", async () => {
  const finish = delayNext(); const view = render(<TelegramRelaySettingsSection active />); await tick();
  view.rerender(<TelegramRelaySettingsSection active={false} />); expect(gets()[0]?.signal?.aborted).toBe(true);
  server.status = { ...server.status, state: "unknown" };
  view.rerender(<TelegramRelaySettingsSection active />); await tick();
  await finish(configured); await tick();
  expect(screen.queryByText("Relay доступен · ключ подтверждён")).not.toBeInTheDocument();
  view.unmount(); expect(vi.getTimerCount()).toBe(0);
});
it("displays only secret configuration presence and safe mapped reasons, never raw tokens or errors", async () => {
  server.status = { ...server.status, secretConfigured: false, reason: { code: "unknown_private_detail", message: "PRIVATE_TOKEN_HMAC_OR_TELEGRAM_URL" } };
  render(<TelegramRelaySettingsSection active />); await tick();
  expect(screen.getByText(/Ключ подписи на основном сервере: не настроен/)).toBeInTheDocument();
  expect(probe()).toBeDisabled(); expect(test()).toBeDisabled();
  expect(screen.getAllByRole("textbox")).toHaveLength(1);
  expect(screen.queryByText(/PRIVATE_TOKEN_HMAC_OR_TELEGRAM_URL/)).not.toBeInTheDocument();
  expect(screen.queryByLabelText(/токен|HMAC/i)).not.toBeInTheDocument();
});
