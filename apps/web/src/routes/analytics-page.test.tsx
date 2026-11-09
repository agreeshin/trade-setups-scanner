import { afterEach, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { AnalyticsPage } from "./analytics-page";

afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

const setFixture = {
  setId: "run-12", kind: "run", numericId: 12, label: "#12 - Всплеск цены и оборота · 1.1.1",
  algorithmId: "price_volume_spike", algorithmVersion: "1.1.1", exchange: "bybit", category: "linear",
  symbol: "BTCUSDT", setupCount: 42, fromUnix: 1_700_000_000, toUnix: 1_700_086_400, createdAt: "2024-01-15T00:00:00Z",
};

const setupA = { exchange: "bybit", category: "linear", symbol: "BTCUSDT", openTimeUnix: 1_700_000_000, closeTimeUnix: 1_700_000_060, closePrice: "100" };
const setupB = { exchange: "bybit", category: "linear", symbol: "ETHUSDT", openTimeUnix: 1_700_003_600, closeTimeUnix: 1_700_003_660, closePrice: "50" };

const selectionMetrics = { totalMatchingSetups: 42, avgSelectionOrdinal: 1.5, avgSelectionDelayMinutes: 90, totalClustersCount: 7, cachedSelection: true };

const shortLevels = Array.from({ length: 14 }, (_, index) => 4 + index * 2);
const longTpLevels = [1, 2, 3, ...shortLevels, 35, 40, 45, 50, 60];
const longSlLevels = [0.1, 0.25, 0.5, 1, 2, 3, ...shortLevels, 35, 40, 45, 50, 60];

function matrixCell(tpPct: number, slPct: number, risk: number | null) {
  return { tpPct, slPct, avgMovePct: risk === null ? null : risk * slPct, riskExpectancy: risk };
}

interface StubOptions {
  sets?: typeof setFixture[];
  shortBulk?: { tpPct: number; slPct: number; avgMovePct: number | null; riskExpectancy: number | null }[];
  longBulk?: { tpPct: number; slPct: number; avgMovePct: number | null; riskExpectancy: number | null }[];
}

function stubFetch(options: StubOptions = {}) {
  const calls: { method: string; path: string }[] = [];
  vi.stubGlobal("fetch", vi.fn((url: string, init?: RequestInit) => {
    const path = url.replace(/^https?:\/\/[^/]+/, "");
    const query = new URLSearchParams(path.split("?")[1] ?? "");
    const method = init?.method ?? "GET";
    calls.push({ method, path: path.split("?")[0]! });
    const json = (body: unknown, status = 200) => Promise.resolve(new Response(JSON.stringify(body), { status }));
    if (path.startsWith("/v1/analytics/sets")) return json({ sets: options.sets ?? [setFixture] });
    if (path.startsWith("/v1/analytics/months")) return json({ months: ["2024-01", "2024-02"] });
    if (path.startsWith("/v1/analytics/setups")) {
      return json({ setups: [setupA, setupB], ...selectionMetrics });
    }
    if (path.startsWith("/v1/analytics/matrix-cells")) {
      const direction = query.get("direction") ?? "short";
      const items = direction === "long" ? options.longBulk ?? [] : options.shortBulk ?? [];
      return json({ items, ...selectionMetrics });
    }
    if (path.startsWith("/v1/analytics/matrix-cell")) {
      const tpPct = Number(query.get("tpPct"));
      const slPct = Number(query.get("slPct"));
      return json({ ...matrixCell(tpPct, slPct, tpPct / slPct), cached: false, cachedSelection: true, closedCount: 4, tpCount: 3, slCount: 1 });
    }
    if (path.startsWith("/v1/analytics/trades/")) {
      const symbol = decodeURIComponent(path.split("/")[4] ?? "");
      const closeTimeUnix = Number(path.split("/")[5]);
      const status = symbol === "BTCUSDT" ? "tp" : "sl";
      return json({
        setup: { ...setupA, symbol, closeTimeUnix },
        direction: "short", tpPct: 4, slPct: 4, status,
        entryPrice: 100, tpPrice: status === "tp" ? 96 : null, slPrice: status === "sl" ? 104 : null,
        closeTimeUnix: closeTimeUnix + 600, riskReturn: status === "tp" ? 1 : -1, cached: false,
      });
    }
    if (path.startsWith("/v1/analytics/trades")) {
      return json({
        items: [{
          setup: setupA, direction: "short", tpPct: 4, slPct: 4, status: "tp",
          entryPrice: 100, tpPrice: 96, slPrice: null, closeTimeUnix: setupA.closeTimeUnix + 600, riskReturn: 1,
        }],
        cachedCount: 1,
      });
    }
    if (path.startsWith("/v1/analytics/cache/summary")) {
      return json({ tables: [{ name: "analytics_matrix_cache", count: 5 }, { name: "analytics_trades_cache", count: 2 }, { name: "analytics_selection_cache", count: 1 }], total: 8 });
    }
    if (path.startsWith("/v1/analytics/cache")) {
      return json({ deleted: [{ name: "analytics_matrix_cache", count: 5 }, { name: "analytics_trades_cache", count: 2 }, { name: "analytics_selection_cache", count: 1 }], total: 8 });
    }
    return json({});
  }));
  return calls;
}

const fullShortGrid = shortLevels.flatMap((tp) => shortLevels.map((sl) => matrixCell(tp, sl, tp === 6 && sl === 6 ? 0.17 : 0.05)));
const fullLongGrid = longTpLevels.flatMap((tp) => longSlLevels.map((sl) => matrixCell(tp, sl, -0.3)));

it("renders the set selector, fully cached matrices and the cache summary", async () => {
  stubFetch({ shortBulk: fullShortGrid, longBulk: fullLongGrid });
  render(<AnalyticsPage />);
  expect(await screen.findByText("#12 - Всплеск цены и оборота · 1.1.1")).toBeInTheDocument();
  expect(await screen.findByText("Сетапов: 42")).toBeInTheDocument();
  await waitFor(() => expect(screen.getAllByText("Ячеек: 196 / 196").length).toBeGreaterThan(0));
  await waitFor(() => expect(screen.getAllByText("Ячеек: 550 / 550").length).toBeGreaterThan(0));
  expect(screen.getAllByText("0.17").length).toBeGreaterThan(0);
  expect(screen.getByText("Всего: 8")).toBeInTheDocument();
  expect(screen.getByText("analytics_matrix_cache")).toBeInTheDocument();
}, 10_000);

it("computes missing matrix cells sequentially after the bulk cache read", async () => {
  stubFetch({ shortBulk: fullShortGrid.slice(0, 1), longBulk: fullLongGrid });
  render(<AnalyticsPage />);
  expect(await screen.findByText("#12 - Всплеск цены и оборота · 1.1.1")).toBeInTheDocument();
  await waitFor(() => expect(screen.getAllByText("Ячеек: 196 / 196").length).toBeGreaterThan(0), { timeout: 15_000 });
}, 30_000);

it("runs the SHORT trades calculation cache-first and shows aggregates", async () => {
  stubFetch();
  render(<AnalyticsPage />);
  await screen.findByText("#12 - Всплеск цены и оборота · 1.1.1");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  expect(await screen.findByText("BTCUSDT")).toBeInTheDocument();
  expect(await screen.findByText("ETHUSDT")).toBeInTheDocument();
  await waitFor(() => expect(screen.getByText("Win rate: 50.0%")).toBeInTheDocument());
  expect(screen.getByText("Матожидание: +0.00R по 2 закрытым")).toBeInTheDocument();
  expect(screen.getAllByText("TP").length).toBeGreaterThan(0);
  expect(screen.getAllByText("SL").length).toBeGreaterThan(0);
}, 10_000);

it("clears the analytics cache and reports the deleted row count", async () => {
  const calls = stubFetch();
  render(<AnalyticsPage />);
  await screen.findByText("Всего: 8");
  await userEvent.click(screen.getByRole("button", { name: "Очистить кеш" }));
  expect(await screen.findByText("Удалено записей: 8")).toBeInTheDocument();
  expect(calls.some((call) => call.method === "DELETE" && call.path === "/v1/analytics/cache")).toBe(true);
});

it("shows the empty state when no finished runs exist", async () => {
  stubFetch({ sets: [] });
  render(<AnalyticsPage />);
  expect(await screen.findByText("Нет наборов для аналитики")).toBeInTheDocument();
});

it("filters trades by the display date range", async () => {
  stubFetch();
  render(<AnalyticsPage />);
  await screen.findByText("#12 - Всплеск цены и оборота · 1.1.1");
  fireEvent.change(screen.getAllByLabelText("С даты")[0]!, { target: { value: "2124-01-01" } });
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  expect(await screen.findByText(/Сделок: 2 \/ 2/)).toBeInTheDocument();
  expect(screen.queryByText("BTCUSDT")).not.toBeInTheDocument();
}, 10_000);
