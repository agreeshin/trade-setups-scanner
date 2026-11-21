import { afterEach, expect, it, vi } from "vitest";
import { fetchSetupDetails, fetchSetupHistory, fetchSetupsAnalytics, fetchSetupsAnalyticsStudies, type SetupHistoryFilter } from "./setups-analytics-api";

const row = { id: 7, exchange: "bybit", symbol: "WATCHUSDT", algorithmId: "price_volume_spike", openTimeUnix: 120, closeTimeUnix: 180, closePrice: "100", recommendation: null, tradeOutcome: null };
const filter: SetupHistoryFilter = { algorithmId: "price_volume_spike", fromUnix: 0, toUnix: 1000, studies: null, q: "%_", hideGateFailed: true };
const json = (value: unknown) => new Response(JSON.stringify(value));
afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("forwards the same literal search to studies, calculation and bounded history without inventing WATCH values", async () => {
  const calls: string[] = [];
  vi.stubGlobal("fetch", vi.fn((path: string) => {
    calls.push(path);
    if (path.startsWith("/v1/setups/analytics/studies")) return Promise.resolve(json({ studies: [] }));
    if (path.startsWith("/v1/setups/analytics?")) return Promise.resolve(json({ totals: { selected: 0, gatePassed: 0, closed: 0, meanR: null }, studies: [], setups: [] }));
    return Promise.resolve(json({ setups: [row], offset: 5000, total: 5001, limit: 25, snapshotId: 6000 }));
  }));
  const signal = new AbortController().signal;
  await fetchSetupsAnalyticsStudies(filter.algorithmId, 0, 1000, signal, filter.q);
  await fetchSetupsAnalytics(filter.algorithmId, 0, 1000, null, signal, filter.q);
  const page = await fetchSetupHistory(filter, 5000, 6000, signal);
  for (const path of calls) expect(new URL(path, "http://fixture").searchParams.get("q")).toBe("%_");
  const params = new URL(calls[2]!, "http://fixture").searchParams;
  expect(params.get("offset")).toBe("5000"); expect(params.get("limit")).toBe("25"); expect(params.has("studies")).toBe(false);
  expect(page.setups[0]?.tradeOutcome).toBeNull(); expect(page.setups[0]?.recommendation).toBeNull();
});

it("distinguishes all, explicit subset and explicit none in history URLs", async () => {
  const calls: string[] = [];
  vi.stubGlobal("fetch", vi.fn((path: string) => { calls.push(path); return Promise.resolve(json({ setups: [], offset: 0, total: 0, limit: 25, snapshotId: 0 })); }));
  for (const studies of [null, [40], []]) await fetchSetupHistory({ ...filter, studies }, 0, undefined, new AbortController().signal);
  expect(calls.map((path) => new URL(path, "http://fixture").searchParams.get("studies"))).toEqual([null, "40", ""]);
});

it("accepts canonical UTF16 entities and nullable unsupported text, but rejects malformed ranges and changed IDs", async () => {
  const text = "🟢 COIN · Алгоритм";
  let message: unknown = { text, entities: [{ type: "code", offset: 3, length: 4 }, { type: "bold", offset: 10, length: 8 }] };
  let responseRow = row;
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ setup: responseRow, telegramMessage: message, reason: "saved_recommendation_unsupported" }))));
  const signal = new AbortController().signal;
  expect((await fetchSetupDetails(7, signal)).telegramMessage).toEqual(message);
  message = null;
  expect((await fetchSetupDetails(7, signal)).telegramMessage).toBeNull();
  for (const entities of [[{ type: "code", offset: 1, length: 1 }], [{ type: "link", offset: 0, length: 2 }], [{ type: "bold", offset: 3, length: 4 }, { type: "code", offset: 4, length: 1 }]]) {
    message = { text, entities };
    await expect(fetchSetupDetails(7, signal)).rejects.toThrow("форматирование");
  }
  message = null; responseRow = { ...row, id: 8 };
  await expect(fetchSetupDetails(7, signal)).rejects.toThrow("идентичность");
});

it("rejects a changed snapshot or wrong page instead of attaching it to current history filters", async () => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ setups: [], offset: 0, total: 0, limit: 25, snapshotId: 11 }))));
  await expect(fetchSetupHistory(filter, 0, 10, new AbortController().signal)).rejects.toThrow("история");
  await expect(fetchSetupHistory(filter, 25, undefined, new AbortController().signal)).rejects.toThrow("история");
});
