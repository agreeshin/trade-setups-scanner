import { afterEach, expect, it, vi } from "vitest";
import { parseSetupTradeOutcome, refreshSetupOutcomesPage, type SetupTradeOutcome } from "./setup-outcomes-api";

const json = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status });
const outcome: SetupTradeOutcome = { status: "tp", entryTimeUnix: 200, entryPrice: 100, gateThreshold: null, tpPrice: 110, slPrice: 95, closeTimeUnix: 300, closePrice: 110, riskReturn: 2, evaluatedAtUnix: 400, observedThroughUnix: 360, isFinal: true };
const page = { snapshotId: 100, observedThroughUnix: 360, processed: 2, computed: 1, updated: 1, nextCursor: null };

afterEach(() => { vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("reads integral outcome metadata and distinguishes missing results from unclosed", () => {
  expect(parseSetupTradeOutcome(outcome)).toEqual(outcome);
  expect(parseSetupTradeOutcome(null)).toBeNull();
  expect(parseSetupTradeOutcome(undefined)).toBeNull();
  expect(parseSetupTradeOutcome({ ...outcome, status: "unclosed", closeTimeUnix: null, closePrice: null, riskReturn: null, isFinal: false })?.status).toBe("unclosed");
});

it.each([
  { status: "other" }, { status: ["tp"] }, { closePrice: Infinity }, { riskReturn: NaN },
  { observedThroughUnix: 1.5 }, { evaluatedAtUnix: undefined }, { isFinal: "true" }, { closeTimeUnix: -1 },
])("rejects malformed persisted fields %j", (invalid) => {
  expect(() => parseSetupTradeOutcome({ ...outcome, ...invalid })).toThrow("Некорректный сохранённый результат");
});

it("posts fifty-row pages using the opaque continuation and authenticated API transport", async () => {
  const fetch = vi.fn<(path: string, init?: RequestInit) => Promise<Response>>(() => Promise.resolve(json(page))); vi.stubGlobal("fetch", fetch);
  expect(await refreshSetupOutcomesPage()).toEqual(page);
  await refreshSetupOutcomesPage("opaque-next");
  expect(fetch).toHaveBeenCalledTimes(2);
  expect(fetch.mock.calls[0]).toEqual(["/v1/setups/outcomes/refresh", expect.objectContaining({ method: "POST", body: JSON.stringify({ limit: 50 }), credentials: "same-origin" })]);
  const init = fetch.mock.calls[1]![1]!;
  expect(init.body).toBe(JSON.stringify({ cursor: "opaque-next", limit: 50 }));
  expect(new Headers(init.headers).get("x-platform-request")).toBe("1");
  expect(init).not.toHaveProperty("keepalive");
});

it.each([
  { snapshotId: -1 }, { processed: 51 }, { computed: 3 }, { updated: 3 },
  { nextCursor: "" }, { nextCursor: "again", processed: 0, computed: 0, updated: 0 }, { nextCursor: "x".repeat(4097) },
])("rejects an invalid or non-progressing page %j", async (invalid) => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ ...page, ...invalid }))));
  await expect(refreshSetupOutcomesPage()).rejects.toThrow("Некорректный ответ обновления");
});

it("rejects a repeated cursor and never falls back to GET after HTTP errors", async () => {
  const fetch = vi.fn(() => Promise.resolve(json({ ...page, nextCursor: "same" }))); vi.stubGlobal("fetch", fetch);
  await expect(refreshSetupOutcomesPage("same")).rejects.toThrow("пакет не подтверждён");
  fetch.mockImplementation(() => Promise.resolve(json({}, 405)));
  await expect(refreshSetupOutcomesPage()).rejects.toThrow("HTTP 405");
  expect(fetch).toHaveBeenCalledTimes(2);
});

it("aborts a stalled page at its finite deadline and removes the timer", async () => {
  vi.useFakeTimers();
  let requestSignal: AbortSignal | null | undefined;
  vi.stubGlobal("fetch", vi.fn((_path: string, init: RequestInit) => new Promise<Response>((_resolve, reject) => {
    requestSignal = init.signal;
    init.signal?.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError")), { once: true });
  })));
  const result = refreshSetupOutcomesPage().catch((error: unknown) => error);
  await vi.advanceTimersByTimeAsync(120_000);
  expect(requestSignal?.aborted).toBe(true);
  const cause = await result;
  expect(cause).toBeInstanceOf(Error);
  if (!(cause instanceof Error)) throw new Error("Expected a timeout error");
  expect(cause.message).toContain("120 секунд");
  expect(vi.getTimerCount()).toBe(0);
});

it("propagates owner cancellation and skips a pre-aborted request", async () => {
  const controller = new AbortController(); controller.abort();
  const fetch = vi.fn(); vi.stubGlobal("fetch", fetch);
  await expect(refreshSetupOutcomesPage(undefined, controller.signal)).rejects.toThrow();
  expect(fetch).not.toHaveBeenCalled();
});
