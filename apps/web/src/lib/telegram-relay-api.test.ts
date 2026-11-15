import { afterEach, expect, it, vi } from "vitest";
import { getTelegramRelay, normalizeTelegramRelayEndpoint, probeTelegramRelay, saveTelegramRelay, testTelegramRelay, type TelegramRelayEnvelope } from "./telegram-relay-api";

const requestId = "d380c45e-89aa-49c8-b6d4-7e4d1c0ead15";
const envelope: TelegramRelayEnvelope = {
  settings: { enabled: true, endpointUrl: "https://relay.example/", revision: 2 },
  status: { endpointUrl: "https://relay.example/", settingsRevision: 2, secretConfigured: true, state: "connected", telegramChannel: "unknown", checkedAt: "2026-11-14T12:00:00Z", reason: null },
};
const json = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status });
afterEach(() => { vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("uses only authenticated main-app routes and exact revision-bound public request bodies", async () => {
  const fetch = vi.fn<typeof globalThis.fetch>().mockResolvedValueOnce(json(envelope)).mockResolvedValueOnce(json(envelope))
    .mockResolvedValueOnce(json(envelope)).mockResolvedValueOnce(json({ requestId, state: "sent", reason: null }));
  vi.stubGlobal("fetch", fetch);
  const signal = new AbortController().signal;
  expect(await getTelegramRelay(signal)).toEqual(envelope);
  await saveTelegramRelay({ ...envelope.settings, endpointUrl: " https://Relay.Example " }, signal);
  await probeTelegramRelay(2, signal);
  expect(await testTelegramRelay(2, requestId, signal)).toEqual({ requestId, state: "sent", reason: null });
  expect(fetch.mock.calls.map(([path, init]) => ({ path, method: init?.method, body: typeof init?.body === "string" ? JSON.parse(init.body) as unknown : null }))).toEqual([
    { path: "/v1/telegram-relay", method: "GET", body: null },
    { path: "/v1/telegram-relay", method: "PUT", body: { enabled: true, endpointUrl: "https://relay.example/", expectedRevision: 2 } },
    { path: "/v1/telegram-relay/probe", method: "POST", body: { expectedRevision: 2 } },
    { path: "/v1/telegram-relay/test", method: "POST", body: { expectedRevision: 2, requestId } },
  ]);
  for (const [, init] of fetch.mock.calls) {
    expect(init?.credentials).toBe("same-origin"); expect(new Headers(init?.headers).get("x-platform-request")).toBe("1");
  }
});

it("accepts disabled unconfigured revision zero and strips undeclared secret fields", async () => {
  const value = { settings: { enabled: false, endpointUrl: "", revision: 0, botToken: "DO_NOT_KEEP" },
    status: { ...envelope.status, endpointUrl: "", settingsRevision: 0, secretConfigured: false, state: "disabled", checkedAt: null, sharedSecret: "DO_NOT_KEEP" } };
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json(value))));
  const result = await getTelegramRelay(new AbortController().signal);
  expect(result.settings).toEqual({ enabled: false, endpointUrl: "", revision: 0 });
  expect(result.status).not.toHaveProperty("sharedSecret");
  await saveTelegramRelay(result.settings, new AbortController().signal);
});

it.each(["https://relay.example", "https://RELAY.example/", " https://relay.example/ "])("canonicalizes HTTPS origin syntax: %s", (value) => {
  expect(normalizeTelegramRelayEndpoint(value)).toBe("https://relay.example/");
});
it.each(["http://relay.example/", "ftp://relay.example/", "relay.example", "https://user:pass@relay.example/", "https://@relay.example/", "https://relay.example/path", "https://relay.example/path/..", "https://relay.example/?q=1", "https://relay.example/?", "https://relay.example/#", "https://relay.example\\path", "https://re\nlay.example/"])("rejects insecure or non-origin endpoint: %s", async (endpointUrl) => {
  const fetch = vi.fn(); vi.stubGlobal("fetch", fetch);
  await expect(saveTelegramRelay({ ...envelope.settings, endpointUrl }, new AbortController().signal)).rejects.toThrow("HTTPS-адрес");
  expect(fetch).not.toHaveBeenCalled();
});

it.each([
  null, {}, { ...envelope, settings: { ...envelope.settings, enabled: "true" } },
  { ...envelope, settings: { ...envelope.settings, endpointUrl: "" } },
  { ...envelope, settings: { ...envelope.settings, revision: -1 } },
  { ...envelope, status: { ...envelope.status, settingsRevision: 0.5 } },
  { ...envelope, status: { ...envelope.status, state: "online" } },
  { ...envelope, status: { ...envelope.status, secretConfigured: "yes" } },
  { ...envelope, status: { ...envelope.status, telegramChannel: true } },
  { ...envelope, status: { ...envelope.status, checkedAt: "0" } },
  { ...envelope, status: { ...envelope.status, checkedAt: "2026-11-14T12:00:00" } },
  { ...envelope, status: { ...envelope.status, reason: { code: "failed", message: "x".repeat(513) } } },
])("rejects unsupported or unbounded DTOs rather than fabricating connected state: %j", async (value) => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json(value))));
  await expect(getTelegramRelay(new AbortController().signal)).rejects.toThrow("Статус не подтверждён");
});

it.each([401, 403, 404, 405, 409, 500, 501])("sanitizes HTTP %i failures without rendering remote messages or credentials", async (status) => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ message: "PRIVATE_BOT_TOKEN_AND_HMAC" }, status))));
  await expect(getTelegramRelay(new AbortController().signal)).rejects.not.toThrow("PRIVATE_BOT_TOKEN_AND_HMAC");
});

it("maps a busy endpoint change to actionable safe advice without retrying or exposing the remote error", async () => {
  const fetch = vi.fn(() => Promise.resolve(json({ error: "telegram_relay_endpoint_busy", message: "PRIVATE_RAW_DIAGNOSTIC" }, 409)));
  vi.stubGlobal("fetch", fetch);
  await expect(saveTelegramRelay({ ...envelope.settings, endpointUrl: "https://new.example/" }, new AbortController().signal))
    .rejects.toThrow("Дождитесь завершения текущих отправок и повторите сохранение");
  expect(fetch).toHaveBeenCalledOnce();
});

it.each(["sent", "unknown", "error"])("retains the exact test acknowledgment state %s", async (state) => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ requestId, state, reason: null }))));
  expect((await testTelegramRelay(2, requestId, new AbortController().signal)).state).toBe(state);
});
it("rejects a successful acknowledgment for a different test request", async () => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ requestId: "different", state: "sent", reason: null }))));
  await expect(testTelegramRelay(2, requestId, new AbortController().signal)).rejects.toThrow("неподдерживаемые данные");
});

it.each(["read", "probe", "test"])("bounds a hanging %s at 10s without retrying it and releases timeout resources", async (kind) => {
  vi.useFakeTimers();
  const fetch = vi.fn<typeof globalThis.fetch>(() => new Promise<Response>(() => undefined)); vi.stubGlobal("fetch", fetch);
  const signal = new AbortController().signal;
  const result = kind === "read" ? getTelegramRelay(signal) : kind === "probe" ? probeTelegramRelay(2, signal) : testTelegramRelay(2, requestId, signal);
  const rejected = expect(result).rejects.toThrow("10 секунд");
  await vi.advanceTimersByTimeAsync(10000); await rejected;
  expect(fetch).toHaveBeenCalledOnce(); expect(fetch.mock.calls[0]?.[1]?.signal?.aborted).toBe(true);
  expect(vi.getTimerCount()).toBe(0);
});
it("aborts an owned request immediately and clears its deadline instead of reporting timeout", async () => {
  vi.useFakeTimers();
  const fetch = vi.fn<typeof globalThis.fetch>(() => new Promise<Response>(() => undefined)); vi.stubGlobal("fetch", fetch);
  const controller = new AbortController();
  const result = getTelegramRelay(controller.signal); const rejected = expect(result).rejects.toMatchObject({ name: "AbortError" });
  controller.abort(); await rejected;
  expect(fetch.mock.calls[0]?.[1]?.signal?.aborted).toBe(true); expect(vi.getTimerCount()).toBe(0);
});
