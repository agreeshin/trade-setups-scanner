import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { SetupsPage } from "./setups-page";
import { RouteActiveContext } from "./route-active";
import type { SetupRow } from "./platform-controls";
import type { SetupTradeOutcomeStatus } from "../lib/setup-outcomes-api";

const row = (id: number): SetupRow => ({ id, exchange: "bybit", category: "linear", symbol: `BTC${id}`, algorithmId: "price_volume_spike", openTimeUnix: 100, closeTimeUnix: 200, closePrice: "100" });
const outcomeRow = (id: number, status: SetupTradeOutcomeStatus): SetupRow => ({ ...row(id), tradeOutcome: {
  status, entryTimeUnix: null, entryPrice: null, gateThreshold: null, tpPrice: null, slPrice: null,
  closeTimeUnix: null, closePrice: null, riskReturn: null, evaluatedAtUnix: 200, observedThroughUnix: 200, isFinal: status === "gate_failed",
} });
const requests: { path: string; signal: AbortSignal | null | undefined }[] = [];
let rows: SetupRow[];
let respond: (params: URLSearchParams) => Response | Promise<Response>;
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });
function page(params: URLSearchParams) {
  const snapshot = Number(params.get("snapshotId") ?? rows[0]?.id ?? 0);
  const filtered = rows.filter((item) => item.id! <= snapshot && item.symbol.includes(params.get("q") ?? "")
    && (params.get("hideGateFailed") !== "true" || item.tradeOutcome?.status !== "gate_failed"));
  const offset = Number(params.get("offset"));
  return json({ setups: filtered.slice(offset, offset + 25), total: filtered.length, offset, limit: 25, snapshotId: snapshot });
}
const calls = () => requests.filter((request) => request.path.startsWith("/v1/setups?"));
const params = () => new URL(calls().at(-1)!.path, "http://localhost").searchParams;
const settle = async () => { await act(async () => { await vi.advanceTimersByTimeAsync(0); }); };
const tick = async (ms = 3000) => { await act(async () => { await vi.advanceTimersByTimeAsync(ms); }); };

beforeEach(() => {
  vi.useFakeTimers(); vi.setSystemTime(new Date("2026-11-13T12:00:00Z")); requests.length = 0;
  rows = Array.from({ length: 60 }, (_, index) => row(60 - index)); respond = page;
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    requests.push({ path, signal: init?.signal });
    if (path === "/v1/setups/outcomes/refresh") return Promise.resolve(json({ snapshotId: 60, observedThroughUnix: 200, processed: 0, computed: 0, updated: 0, nextCursor: null }));
    if (path === "/v1/settings/ui") return Promise.resolve(json({ value: { setupFilter: "BTC" } }));
    if (path === "/v1/algorithms") return Promise.resolve(json({ algorithms: [] }));
    return Promise.resolve(respond(new URL(path, "http://localhost").searchParams));
  }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("defaults the server gate filter on, hides only saved gate failures and restores them when unchecked", async () => {
  rows = [outcomeRow(7, "gate_failed"), outcomeRow(6, "tp"), outcomeRow(5, "sl"), outcomeRow(4, "unclosed"), outcomeRow(3, "no_data"), { ...row(2), tradeOutcome: null }, row(1)];
  render(<SetupsPage />); await settle();
  const checkbox = screen.getByRole("checkbox", { name: "Скрыть сетапы, которые не прошли гейт" });
  expect(checkbox).toBeChecked(); expect(params().get("hideGateFailed")).toBe("true");
  expect(screen.getByText("Страница 1 из 1 · Найдено: 6")).toBeInTheDocument();
  expect(screen.queryByText("bybit · BTC7")).not.toBeInTheDocument();
  for (const id of [6, 5, 4, 3, 2, 1]) expect(screen.getByText(`bybit · BTC${id}`)).toBeInTheDocument();
  const refreshCalls = requests.filter((request) => request.path === "/v1/setups/outcomes/refresh").length;
  fireEvent.click(checkbox); await settle();
  expect(checkbox).not.toBeChecked(); expect(params().get("hideGateFailed")).toBe("false");
  expect(screen.getByText("bybit · BTC7")).toBeInTheDocument();
  expect(screen.getByText("Страница 1 из 1 · Найдено: 7")).toBeInTheDocument();
  await tick(); expect(params().get("hideGateFailed")).toBe("false"); expect(params().get("q")).toBe("BTC");
  fireEvent.change(screen.getByLabelText("Инструмент, биржа или алгоритм"), { target: { value: "BTC7" } }); await settle();
  expect(params().get("hideGateFailed")).toBe("false"); expect(screen.getByText("bybit · BTC7")).toBeInTheDocument();
  fireEvent.click(checkbox); await settle();
  expect(checkbox).toBeChecked(); expect(screen.queryByText("bybit · BTC7")).not.toBeInTheDocument();
  expect(requests.filter((request) => request.path === "/v1/setups/outcomes/refresh")).toHaveLength(refreshCalls);
});

it("filters before pagination and keeps historical snapshots pinned when toggling the gate filter", async () => {
  rows = rows.map((item) => item.id! >= 35 ? outcomeRow(item.id!, "gate_failed") : item);
  render(<SetupsPage />); await settle();
  expect(screen.getByText("Страница 1 из 2 · Найдено: 34")).toBeInTheDocument();
  expect(screen.getByText("bybit · BTC34")).toBeInTheDocument(); expect(screen.getByText("bybit · BTC10")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Далее" })); await settle();
  expect(params().get("offset")).toBe("25"); expect(params().get("snapshotId")).toBe("60");
  expect(params().get("hideGateFailed")).toBe("true"); expect(screen.getByText("bybit · BTC9")).toBeInTheDocument();
  expect(screen.getByText("Страница 2 из 2 · Найдено: 34")).toBeInTheDocument();
  rows.unshift(row(61));
  fireEvent.click(screen.getByRole("checkbox", { name: "Скрыть сетапы, которые не прошли гейт" })); await settle();
  expect(params().get("offset")).toBe("0"); expect(params().get("snapshotId")).toBe("60"); expect(params().get("q")).toBe("BTC");
  expect(screen.getByText("Страница 1 из 3 · Найдено: 60")).toBeInTheDocument(); expect(screen.getByText("bybit · BTC60")).toBeInTheDocument();
  expect(screen.queryByText("bybit · BTC61")).not.toBeInTheDocument();
  const historicalCalls = calls().length; await tick(9000); expect(calls()).toHaveLength(historicalCalls);
  fireEvent.click(screen.getByRole("button", { name: "Следить за live-лентой" })); await settle();
  expect(params().get("hideGateFailed")).toBe("false"); expect(params().has("snapshotId")).toBe(false);
  expect(screen.getByText("bybit · BTC61")).toBeInTheDocument();
});

it("keeps a paused feed paused when changing the gate filter or refreshing its page", async () => {
  render(<SetupsPage />); await settle();
  fireEvent.click(screen.getByRole("button", { name: "Приостановить обновление" })); await settle();
  fireEvent.click(screen.getByRole("checkbox", { name: "Скрыть сетапы, которые не прошли гейт" })); await settle();
  expect(params().get("snapshotId")).toBe("60"); expect(params().get("hideGateFailed")).toBe("false");
  const pausedCalls = calls().length; await tick(9000); expect(calls()).toHaveLength(pausedCalls);
  fireEvent.click(screen.getByRole("button", { name: "Обновить" })); await settle();
  expect(params().get("hideGateFailed")).toBe("false"); expect(params().has("snapshotId")).toBe(false);
  const refreshedCalls = calls().length; await tick(9000); expect(calls()).toHaveLength(refreshedCalls);
});

it("follows the fresh first page every three seconds, preserving the filter and deduplicating IDs", async () => {
  render(<SetupsPage />); await settle();
  expect(calls()).toHaveLength(1); expect(params().get("q")).toBe("BTC"); expect(params().has("snapshotId")).toBe(false);
  rows = [row(61), row(61), ...rows]; await tick();
  expect(calls()).toHaveLength(2); expect(screen.getAllByText("bybit · BTC61")).toHaveLength(1);
  expect(params().get("offset")).toBe("0"); expect(params().get("q")).toBe("BTC"); expect(params().has("snapshotId")).toBe(false);
  expect(screen.getByText(/^Последнее успешное чтение:/)).toBeInTheDocument();
});

it("pauses live refresh and explicitly resumes with a fresh snapshot", async () => {
  render(<SetupsPage />); await settle();
  fireEvent.click(screen.getByRole("button", { name: "Приостановить обновление" })); await settle();
  const pausedCalls = calls().length; rows.unshift(row(61)); await tick(9000);
  expect(calls()).toHaveLength(pausedCalls); expect(screen.queryByText("bybit · BTC61")).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Следить за live-лентой" })); await settle();
  expect(screen.getByText("bybit · BTC61")).toBeInTheDocument(); expect(params().has("snapshotId")).toBe(false);
  await tick(); expect(calls()).toHaveLength(pausedCalls + 2);
});

it("pins historical pages during background activity and preserves explicit Refresh as latest first page", async () => {
  render(<SetupsPage />); await settle();
  fireEvent.click(screen.getByRole("button", { name: "Далее" })); await settle();
  expect(params().get("offset")).toBe("25"); expect(params().get("snapshotId")).toBe("60");
  const historicalCalls = calls().length; rows.unshift(row(61)); await tick(9000);
  expect(calls()).toHaveLength(historicalCalls); expect(screen.getByText("bybit · BTC35")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Назад" })); await settle();
  expect(params().get("snapshotId")).toBe("60"); expect(screen.queryByText("bybit · BTC61")).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Следить за live-лентой" })); await settle();
  expect(params().get("offset")).toBe("0"); expect(params().has("snapshotId")).toBe(false); expect(screen.getByText("bybit · BTC61")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Далее" })); await settle();
  fireEvent.click(screen.getByRole("button", { name: "Обновить" })); await settle();
  expect(params().get("offset")).toBe("0"); expect(params().get("q")).toBe("BTC"); expect(params().has("snapshotId")).toBe(false);
  expect(screen.getByText("bybit · BTC61")).toBeInTheDocument();
});

it("retains the last confirmed rows and read time on transient failure and recovers automatically", async () => {
  render(<SetupsPage />); await settle();
  const lastRead = screen.getByText(/^Последнее успешное чтение:/).textContent;
  respond = () => json({}, 503); await tick();
  expect(screen.getByText("bybit · BTC60")).toBeInTheDocument();
  expect(screen.getByRole("alert")).toHaveTextContent("HTTP 503"); expect(screen.getByRole("alert")).toHaveTextContent("устареть");
  expect(screen.getByText(/^Последнее успешное чтение:/).textContent).toBe(lastRead);
  expect(screen.queryByText("Сетапы не найдены.")).not.toBeInTheDocument();
  respond = page; rows.unshift(row(61)); await tick();
  expect(screen.getByText("bybit · BTC61")).toBeInTheDocument(); expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(screen.getByText(/^Последнее успешное чтение:/).textContent).not.toBe(lastRead);
});

it("serializes slow reads, aborts inactive and hidden reads, and refreshes on activation without losing the filter", async () => {
  const view = render(<RouteActiveContext.Provider value><SetupsPage /></RouteActiveContext.Provider>); await settle();
  let finish: ((response: Response) => void) | undefined;
  respond = () => new Promise<Response>((resolve) => { finish = resolve; }); await tick();
  expect(calls()).toHaveLength(2); const slowSignal = calls().at(-1)?.signal;
  await tick(9000); expect(calls()).toHaveLength(2);
  view.rerender(<RouteActiveContext.Provider value={false}><SetupsPage /></RouteActiveContext.Provider>); await settle();
  expect(slowSignal?.aborted).toBe(true); await tick(9000); expect(calls()).toHaveLength(2);
  respond = page; rows.unshift(row(61));
  view.rerender(<RouteActiveContext.Provider value><SetupsPage /></RouteActiveContext.Provider>); await settle();
  expect(calls()).toHaveLength(3); expect(params().get("q")).toBe("BTC"); expect(screen.getByText("bybit · BTC61")).toBeInTheDocument();
  await act(async () => { finish?.(json({ setups: [], total: 0, offset: 0, limit: 25, snapshotId: 0 })); await Promise.resolve(); });
  expect(screen.getByText("bybit · BTC61")).toBeInTheDocument();
  const visibility = vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
  fireEvent(document, new Event("visibilitychange")); await tick(9000); expect(calls()).toHaveLength(3);
  expect(calls().at(-1)?.signal?.aborted).toBe(true);
  visibility.mockReturnValue("visible"); fireEvent(document, new Event("visibilitychange")); await settle();
  expect(calls()).toHaveLength(4); expect(params().has("snapshotId")).toBe(false);
  view.unmount(); expect(calls().at(-1)?.signal?.aborted).toBe(true); expect(vi.getTimerCount()).toBe(0);
  expect(requests.filter((request) => request.path === "/v1/settings/ui")).toHaveLength(1);
});

it("schedules the next refresh only after a slow response settles", async () => {
  render(<SetupsPage />); await settle();
  let finish: ((response: Response) => void) | undefined;
  respond = () => new Promise<Response>((resolve) => { finish = resolve; }); await tick(); await tick(9000);
  expect(calls()).toHaveLength(2);
  respond = page; await act(async () => { finish?.(page(new URLSearchParams({ offset: "0", q: "BTC" }))); await Promise.resolve(); });
  await tick(2999); expect(calls()).toHaveLength(2); await tick(1); expect(calls()).toHaveLength(3);
});

it("does not show a successful empty feed when the first read fails", async () => {
  respond = () => json({}, 503); render(<SetupsPage />); await settle();
  expect(screen.getByRole("alert")).toHaveTextContent("Лента недоступна");
  expect(screen.queryByText("Сетапы не найдены.")).not.toBeInTheDocument();
  respond = page; await tick(); expect(screen.getByText("bybit · BTC60")).toBeInTheDocument();
});

it("keeps the feed panel alive behind the analytics sidebar section", async () => {
  render(<SetupsPage />); await settle();
  expect(screen.getByRole("tab", { name: "Лента" })).toHaveAttribute("aria-selected", "true");
  expect(screen.getByRole("tabpanel", { name: "Лента" })).toBeVisible();
  fireEvent.click(screen.getByRole("tab", { name: "Аналитика" })); await settle(); await tick(400);
  expect(requests.some((request) => request.path === "/v1/algorithms")).toBe(true);
  expect(screen.queryByRole("tabpanel", { name: "Лента" })).not.toBeInTheDocument();
  expect(screen.getByRole("tabpanel", { name: "Аналитика" })).toBeVisible();
  expect(screen.getByText("bybit · BTC60")).toBeInTheDocument();
  const feedCallsBefore = calls().length;
  fireEvent.click(screen.getByRole("tab", { name: "Лента" })); await settle();
  expect(screen.queryByRole("tabpanel", { name: "Аналитика" })).not.toBeInTheDocument();
  expect(screen.getByRole("tabpanel", { name: "Лента" })).toBeVisible();
  expect(screen.getByText("bybit · BTC60")).toBeInTheDocument();
  await tick(3000);
  expect(calls().length).toBeGreaterThan(feedCallsBefore);
});

it("rereads the current historical query after manual outcome refresh without resetting the snapshot or filter", async () => {
  render(<SetupsPage />); await settle();
  fireEvent.click(screen.getByRole("button", { name: "Далее" })); await settle();
  expect(params().get("offset")).toBe("25"); expect(params().get("snapshotId")).toBe("60");
  const originalFetch = globalThis.fetch;
  const refresh = vi.fn(() => Promise.resolve(json({ snapshotId: 60, observedThroughUnix: 200, processed: 1, computed: 1, updated: 1, nextCursor: null })));
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => path === "/v1/setups/outcomes/refresh" ? refresh() : originalFetch(path, init)));
  const before = calls().length;
  const icon = screen.getByRole("button", { name: "Обновить результаты сетапов" });
  fireEvent.focus(icon); expect(screen.getByRole("tooltip")).toHaveTextContent("Обновить результаты сетапов");
  fireEvent.click(icon); fireEvent.click(icon); await settle();
  expect(refresh).toHaveBeenCalledTimes(1);
  expect(calls()).toHaveLength(before + 1);
  expect(params().get("offset")).toBe("25"); expect(params().get("snapshotId")).toBe("60"); expect(params().get("q")).toBe("BTC");
  expect(screen.getByText("bybit · BTC35")).toBeInTheDocument();
  expect(icon).toHaveAttribute("aria-busy", "false"); expect(icon).toBeEnabled();
});
