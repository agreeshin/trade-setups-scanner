import { StrictMode } from "react";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { applyInterfaceSettings, DEFAULT_INTERFACE_SETTINGS } from "../lib/datetime-format";
import type { SetupsAnalyticsResult } from "../lib/setups-analytics-api";
import type { AdaptiveRecommendation } from "../lib/setup-recommendation";
import type { SetupTradeOutcomeStatus } from "../lib/setup-outcomes-api";
import { SetupsPage } from "./setups-page";
import { RouteActiveContext } from "./route-active";
import type { SetupRow } from "./platform-controls";

const loadedAt = Date.parse("2026-11-21T12:34:56Z") / 1000;
const row = (id: number): SetupRow => ({ id, exchange: "bybit", category: "linear", symbol: `BTC${id}`, algorithmId: "price_volume_spike",
  openTimeUnix: loadedAt - 3660, closeTimeUnix: loadedAt - 3600, closePrice: "100", recommendation: null, tradeOutcome: null });
const recommendation: AdaptiveRecommendation = { direction: "short", tpPct: 10, slPct: 5, rawR: 2, meanR: 0.3, robustScore: 0.2,
  timeShiftMinutes: 0, priceChangePct: null, externalName: "Сохранённый алгоритм", studyId: 40, generationId: 36, slotId: 1, ownerId: "admin",
  clusterEnabled: false, clusterGapMinutes: null, params: {}, provenance: { algorithmId: "price_volume_spike", algorithmVersion: "1",
    optimizerVersion: "fixture", seed: 1, snapshotHash: "saved", capturedAt: "2026-11-20T00:00:00Z", fromUnix: loadedAt - 86400,
    toUnix: loadedAt - 3600, observationCutoff: loadedAt - 3600 } };
const outcomeRow = (id: number, status: SetupTradeOutcomeStatus): SetupRow => ({ ...row(id), recommendation, algorithmVersion: "snapshot-v1", params: { windowMinutes: 5 }, tradeOutcome: {
  status, entryTimeUnix: loadedAt - 3600, entryPrice: 100, gateThreshold: null, tpPrice: 90, slPrice: 105,
  closeTimeUnix: status === "tp" ? loadedAt - 1800 : null, closePrice: status === "tp" ? 90 : null, riskReturn: status === "tp" ? 2 : null,
  evaluatedAtUnix: loadedAt, observedThroughUnix: loadedAt - 60, isFinal: status === "tp" || status === "gate_failed",
} });
const analyticsResult: SetupsAnalyticsResult = { totals: { selected: 2, gatePassed: 1, closed: 1, meanR: 2 }, studies: [], setups: [{
  id: 6, symbol: "BTC6", exchange: "bybit", slotId: 1, studyId: 40, direction: "short", setupTimeUnix: loadedAt - 3600,
  entryTimeUnix: loadedAt - 3600, entryPrice: 100, gateThreshold: null, tpPrice: 90, slPrice: 105, closeTimeUnix: loadedAt - 1800, closePrice: 90, riskReturn: 2, status: "tp",
}] };
const options = { studies: [{ studyId: 40, externalName: "Исходное исследование", setupCount: 2, generationId: 36, artifactId: null }] };
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });
const refreshed = () => json({ snapshotId: 60, observedThroughUnix: loadedAt - 60, processed: 1, computed: 1, updated: 1, nextCursor: null });
const requests: { path: string; method: string; signal: AbortSignal | null | undefined }[] = [];
let rows: SetupRow[];
let total: number | undefined;
let refreshResponse: () => Response | Promise<Response>;
let studiesResponse: () => Response | Promise<Response>;
let analyticsResponse: (params: URLSearchParams) => Response | Promise<Response>;
let historyResponse: (params: URLSearchParams) => Response | Promise<Response>;
function historyPage(params: URLSearchParams) {
  const snapshot = Number(params.get("snapshotId") ?? Math.max(0, ...rows.map((item) => item.id!)));
  const query = (params.get("q") ?? "").toLowerCase();
  const filtered = rows.filter((item) => item.id! <= snapshot && `${item.symbol} ${item.exchange} ${item.algorithmId}`.toLowerCase().includes(query)
    && (params.get("hideGateFailed") !== "true" || item.tradeOutcome?.status !== "gate_failed"));
  const offset = Number(params.get("offset"));
  return json({ setups: filtered.slice(offset, offset + 25), total: total ?? filtered.length, offset, limit: 25, snapshotId: snapshot });
}
const refreshes = () => requests.filter((request) => request.path === "/v1/setups/outcomes/refresh");
const calculations = () => requests.filter((request) => request.path.startsWith("/v1/setups/analytics?") && request.method === "POST");
const studyReads = () => requests.filter((request) => request.path.startsWith("/v1/setups/analytics/studies?"));
const historyReads = () => requests.filter((request) => request.path.startsWith("/v1/setups?"));
const params = (request: { path: string }) => new URL(request.path, "http://localhost").searchParams;
const settle = async (ms = 0) => { await act(async () => { await vi.advanceTimersByTimeAsync(ms); }); };
const history = () => within(screen.getByRole("region", { name: "История сохранённых сетапов" }));

beforeEach(() => {
  vi.useFakeTimers(); vi.setSystemTime(loadedAt * 1000); requests.length = 0; total = undefined;
  window.localStorage.clear(); applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "UTC" });
  rows = [outcomeRow(7, "gate_failed"), outcomeRow(6, "tp"), row(5)];
  refreshResponse = refreshed; studiesResponse = () => json(options); analyticsResponse = () => json(analyticsResult); historyResponse = historyPage;
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    requests.push({ path, method: init?.method ?? "GET", signal: init?.signal });
    if (path === "/v1/setups/outcomes/refresh") return Promise.resolve(refreshResponse());
    if (path === "/v1/settings/ui") return Promise.resolve(json({ value: { setupFilter: "BTC" } }));
    if (path === "/v1/settings/web-analytics") return Promise.resolve(json({ settings: { trendMode: "mean" } }));
    if (path === "/v1/algorithms") return Promise.resolve(json({ algorithms: [{ id: "price_volume_spike", name: "Рост цены и объёма", version: "1", description: "", enabled: true, params: {}, fields: [] }] }));
    if (path.startsWith("/v1/setups/analytics/studies?")) return Promise.resolve(studiesResponse());
    if (path.startsWith("/v1/setups/analytics?")) return Promise.resolve(analyticsResponse(params({ path })));
    if (path.startsWith("/v1/setups?")) return Promise.resolve(historyResponse(params({ path })));
    const detail = /^\/v1\/setups\/(\d+)\/details$/.exec(path);
    if (detail) {
      const setup = rows.find((item) => item.id === Number(detail[1]));
      const text = "СЕРВЕРНЫЙ SHORT BTC6\nУсловия: 123.456 USDT";
      return Promise.resolve(json({ setup, telegramMessage: setup?.recommendation ? { text, entities: [{ type: "bold", offset: 0, length: 9 }, { type: "code", offset: text.indexOf("123.456"), length: 7 }] } : null, reason: "legacy_watch" }));
    }
    throw new Error(`Unexpected Setups request: ${path}`);
  }));
});
afterEach(() => { cleanup(); applyInterfaceSettings(DEFAULT_INTERFACE_SETTINGS); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("orders the sole StrictMode first calculation after outcome refresh and fresh options, without Feed or sidebar", async () => {
  let finishRefresh: ((response: Response) => void) | undefined;
  let finishStudies: ((response: Response) => void) | undefined;
  refreshResponse = () => new Promise<Response>((resolve) => { finishRefresh = resolve; });
  studiesResponse = () => new Promise<Response>((resolve) => { finishStudies = resolve; });
  render(<StrictMode><SetupsPage /></StrictMode>); await settle();
  expect(refreshes()).toHaveLength(1); expect(studyReads()).toHaveLength(0); expect(calculations()).toHaveLength(0);
  expect(screen.getByRole("button", { name: "Обновить результаты сетапов" })).toBeDisabled();
  expect(screen.queryByRole("tab")).not.toBeInTheDocument();
  expect(document.querySelector('[data-slot="page-sidebar"]')).toBeNull();
  expect(screen.queryByRole("heading", { name: "Аналитика выпущенных сетапов" })).not.toBeInTheDocument();
  await act(async () => { finishRefresh?.(refreshed()); await Promise.resolve(); }); await settle();
  expect(studyReads()).toHaveLength(1); expect(calculations()).toHaveLength(0);
  expect(params(studyReads()[0]!).get("toUnix")).toBe(String(loadedAt));
  expect(params(studyReads()[0]!).get("q")).toBe("BTC");
  await act(async () => { finishStudies?.(json(options)); await Promise.resolve(); }); await settle();
  expect(calculations()).toHaveLength(1);
  expect(requests.indexOf(refreshes()[0]!)).toBeLessThan(requests.indexOf(studyReads()[0]!));
  expect(requests.indexOf(studyReads()[0]!)).toBeLessThan(requests.indexOf(calculations()[0]!));
  expect(screen.getByTestId("setups-analytics-mean")).toHaveTextContent("+2.00R");
  await settle(9000); expect(refreshes()).toHaveLength(1); expect(calculations()).toHaveLength(1);
});

it("does not calculate or automatically repeat a failed initial refresh; explicit recovery completes the first calculation", async () => {
  refreshResponse = () => json({}, 503);
  render(<SetupsPage />); await settle();
  expect(screen.getByRole("alert")).toHaveTextContent("HTTP 503");
  expect(screen.getByRole("alert")).toHaveTextContent("вручную");
  expect(screen.getByRole("button", { name: "Рассчитать" })).toBeDisabled();
  await settle(9000);
  expect(refreshes()).toHaveLength(1); expect(studyReads()).toHaveLength(0); expect(calculations()).toHaveLength(0);
  refreshResponse = refreshed;
  const refresh = screen.getByRole("button", { name: "Обновить результаты сетапов" });
  fireEvent.click(refresh); fireEvent.click(refresh); await settle();
  expect(refreshes()).toHaveLength(2); expect(calculations()).toHaveLength(1);
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(screen.getByTestId("setups-analytics-mean")).toHaveTextContent("+2.00R");
});

it("keeps a fixed paginated snapshot across time and route returns, without follow-live or repeated mutation", async () => {
  rows = Array.from({ length: 60 }, (_, index) => row(60 - index));
  const view = render(<RouteActiveContext.Provider value><SetupsPage /></RouteActiveContext.Provider>); await settle();
  fireEvent.click(history().getByRole("button", { name: "Далее" })); await settle();
  expect(params(historyReads().at(-1)!).get("offset")).toBe("25");
  expect(params(historyReads().at(-1)!).get("snapshotId")).toBe("60");
  const before = historyReads().length; rows.unshift(row(61)); await settle(9000);
  expect(historyReads()).toHaveLength(before); expect(history().queryByText("BTC61")).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: /Следить за live|Приостановить обновление/ })).not.toBeInTheDocument();
  view.rerender(<RouteActiveContext.Provider value={false}><SetupsPage /></RouteActiveContext.Provider>); await settle(9000);
  view.rerender(<RouteActiveContext.Provider value><SetupsPage /></RouteActiveContext.Provider>); await settle();
  expect(params(historyReads().at(-1)!).get("snapshotId")).toBe("60");
  expect(params(historyReads().at(-1)!).get("offset")).toBe("25");
  expect(history().getByText("BTC35")).toBeInTheDocument();
  expect(refreshes()).toHaveLength(1); expect(calculations()).toHaveLength(1);
  view.unmount(); await settle(); expect(vi.getTimerCount()).toBe(0);
});

it("keeps WATCH/null outcomes accessible, while the gate Switch changes only displayed rows and never numeric totals", async () => {
  render(<SetupsPage />); await settle();
  const control = screen.getByRole("switch", { name: "Скрыть сетапы, которые не прошли гейт" });
  const controlsRow = screen.getByTestId("setups-history-controls");
  expect(controlsRow).toHaveClass("flex", "w-full", "min-w-0", "justify-end");
  expect(controlsRow).toContainElement(control);
  expect(controlsRow.nextElementSibling).toBe(screen.getByRole("region", { name: "История сохранённых сетапов" }));
  expect(screen.getByLabelText("Алгоритм").closest('[data-slot="card"]')).not.toContainElement(control);
  expect(control).toBeChecked(); expect(history().queryByText("BTC7")).not.toBeInTheDocument();
  const watch = within(history().getByText("BTC5").closest("tr")!);
  expect(watch.getByText("WATCH")).toBeInTheDocument(); expect(watch.getByText("Неприменимо")).toHaveAttribute("data-slot", "badge");
  expect(watch.queryByText(/LONG|SHORT|[+-]?0\.00\s*R/)).not.toBeInTheDocument();
  expect(watch.getByRole("button", { name: "Подробности BTC5" })).toBeEnabled();
  expect(within(history().getByText("BTC6").closest("tr")!).getByText("SHORT")).toBeInTheDocument();
  const beforeFilter = params(historyReads().at(-1)!);
  const observedChart = screen.getByTestId("setups-daily-outcomes");
  const originalPoints = observedChart.querySelector("circle")?.outerHTML;
  fireEvent.click(control); await settle();
  expect(control).not.toBeChecked(); expect(history().getByText("BTC7")).toBeInTheDocument();
  expect(screen.getByTestId("setups-analytics-mean")).toHaveTextContent("+2.00R");
  expect(screen.getByLabelText("Прошли гейт: 1 из 2")).toBeInTheDocument();
  expect(refreshes()).toHaveLength(1); expect(calculations()).toHaveLength(1);
  expect(params(historyReads().at(-1)!).get("hideGateFailed")).toBe("false");
  const afterFilter = params(historyReads().at(-1)!);
  for (const key of ["algorithmId", "fromUnix", "toUnix", "q", "studies"]) expect(afterFilter.get(key)).toBe(beforeFilter.get(key));
  expect(screen.getByTestId("setups-daily-outcomes")).toBe(observedChart);
  expect(observedChart.querySelector("circle")?.outerHTML).toBe(originalPoints);
  fireEvent.click(watch.getByRole("button", { name: "Подробности BTC5" })); await settle();
  expect(screen.getByRole("dialog", { name: "Подробности BTC5" })).toHaveTextContent("legacy_watch");
  expect(calculations()).toHaveLength(1);
});

it("renders the canonical server Telegram text above saved details, never a reconstructed client caption", async () => {
  render(<SetupsPage />); await settle();
  const trigger = history().getByRole("button", { name: "Подробности BTC6" });
  fireEvent.click(trigger); await settle();
  const dialog = screen.getByRole("dialog", { name: "Подробности BTC6" });
  const text = dialog.querySelector("pre")!;
  expect(text.textContent).toBe("СЕРВЕРНЫЙ SHORT BTC6\nУсловия: 123.456 USDT");
  expect(within(text).getByText("СЕРВЕРНЫЙ", { selector: "strong" })).toBeInTheDocument();
  expect(within(text).getByText("123.456", { selector: "code" })).toBeInTheDocument();
  expect(within(dialog).getByText("snapshot-v1")).toBeInTheDocument();
  expect(text.compareDocumentPosition(within(dialog).getByRole("heading", { name: "Сохранённые подробности" })) & Node.DOCUMENT_POSITION_FOLLOWING).not.toBe(0);
  expect(requests.filter((request) => request.method !== "GET")).toHaveLength(2);
});

it("applies search to options, history and the next explicit calculation identity, retaining the global unfiltered CSV link", async () => {
  render(<SetupsPage />); await settle();
  const csv = screen.getByRole("link", { name: "Экспорт CSV (без фильтра)" });
  expect(csv).toHaveAttribute("href", "/v1/export/setups.csv");
  const search = screen.getByRole("textbox", { name: "Инструмент, биржа или алгоритм" });
  fireEvent.change(search, { target: { value: "BTC6_%" } }); await settle();
  expect(calculations()).toHaveLength(1);
  expect(params(studyReads().at(-1)!).get("q")).toBe("BTC6_%");
  expect(params(historyReads().at(-1)!).get("q")).toBe("BTC6_%");
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" })); await settle();
  expect(calculations()).toHaveLength(2); expect(params(calculations()[1]!).get("q")).toBe("BTC6_%");
  expect(params(calculations()[1]!).get("toUnix")).toBe(String(loadedAt));
  expect(csv).toHaveAttribute("href", "/v1/export/setups.csv");
  expect(refreshes()).toHaveLength(1);
});

it("aborts an obsolete calculation when filters change and rejects its late result after the new explicit request finishes", async () => {
  let finishOld: ((response: Response) => void) | undefined;
  analyticsResponse = (query) => query.get("q") === "BTC6"
    ? json({ ...analyticsResult, totals: { ...analyticsResult.totals, meanR: -1 }, setups: analyticsResult.setups.map((setup) => ({ ...setup, status: "sl", riskReturn: -1 })) })
    : new Promise<Response>((resolve) => { finishOld = resolve; });
  render(<SetupsPage />); await settle();
  expect(calculations()).toHaveLength(1);
  const original = calculations()[0]!;
  expect(original.signal?.aborted).toBe(false);
  fireEvent.change(screen.getByRole("textbox", { name: "Инструмент, биржа или алгоритм" }), { target: { value: "BTC6" } }); await settle();
  expect(original.signal?.aborted).toBe(true);
  expect(calculations()).toHaveLength(1);
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" })); await settle();
  expect(calculations()).toHaveLength(2);
  expect(params(calculations()[1]!).get("q")).toBe("BTC6");
  expect(screen.getByTestId("setups-analytics-mean")).toHaveTextContent("-1.00R");
  await act(async () => { finishOld?.(json(analyticsResult)); await Promise.resolve(); }); await settle();
  expect(screen.getByTestId("setups-analytics-mean")).toHaveTextContent("-1.00R");
  expect(screen.getByTestId("setups-daily-outcomes").querySelector('circle[data-mean-r="-1.00"]')).not.toBeNull();
  expect(refreshes()).toHaveLength(1);
});

it("keeps history pagination available when numeric analysis rejects more than 5000 setups, without partial or zero success", async () => {
  total = 5001; rows = Array.from({ length: 60 }, (_, index) => row(60 - index));
  analyticsResponse = () => json({ error: "setups_analytics_limit_exceeded" }, 400);
  render(<SetupsPage />); await settle();
  expect(screen.getByRole("alert")).toHaveTextContent("5000");
  expect(screen.queryByTestId("setups-analytics-mean")).not.toBeInTheDocument();
  expect(history().getByText("BTC60")).toBeInTheDocument();
  fireEvent.click(history().getByRole("button", { name: "Далее" })); await settle();
  expect(history().getByText("BTC35")).toBeInTheDocument();
  expect(history().getByRole("button", { name: "Подробности BTC35" })).toBeEnabled();
  expect(params(historyReads().at(-1)!).get("offset")).toBe("25");
  await settle(9000); expect(calculations()).toHaveLength(1); expect(refreshes()).toHaveLength(1);
  expect(screen.getByRole("link", { name: "Экспорт CSV (без фильтра)" })).toHaveAttribute("href", "/v1/export/setups.csv");
});
