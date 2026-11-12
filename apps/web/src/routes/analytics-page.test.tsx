import { afterEach, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { AnalyticsPage } from "./analytics-page";

afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

const setFixture = {
  setId: "run-12", kind: "run", numericId: 12, label: "#12 · Утренний прогон",
  algorithmId: "price_volume_spike", algorithmVersion: "1.1.1", params: { priceGrowthPct: "2.5", windowMinutes: 30, volumeMultiplier: "3" },
  exchange: "bybit", category: "linear", symbol: "BTCUSDT", setupCount: 42, fromUnix: 1_700_000_000, toUnix: 1_700_086_400, createdAt: "2024-01-15T00:00:00Z",
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
  interceptMatrixGrid?: (direction: string) => Promise<Response> | undefined;
  savedSettings?: Record<string, unknown>;
}

function stubFetch(options: StubOptions = {}) {
  const calls: { method: string; path: string; search: string; body?: string }[] = [];
  vi.stubGlobal("fetch", vi.fn((url: string, init?: RequestInit) => {
    const path = url.replace(/^https?:\/\/[^/]+/, "");
    const query = new URLSearchParams(path.split("?")[1] ?? "");
    const method = init?.method ?? "GET";
    calls.push({ method, path: path.split("?")[0]!, search: path.split("?")[1] ?? "", ...(typeof init?.body === "string" ? { body: init.body } : {}) });
    const json = (body: unknown, status = 200) => Promise.resolve(new Response(JSON.stringify(body), { status }));
    if (path.startsWith("/v1/settings/web-analytics")) {
      if (method === "PUT") return json({ settings: typeof init?.body === "string" ? (JSON.parse(init.body) as unknown) : {} });
      return json({ settings: options.savedSettings ?? {} });
    }
    if (path.startsWith("/v1/analytics/sets")) return json({ sets: options.sets ?? [setFixture] });
    if (path.startsWith("/v1/analytics/months")) return json({ months: ["2024-01", "2024-02"] });
    if (path.startsWith("/v1/analytics/setups")) {
      return json({ setups: [setupA, setupB], ...selectionMetrics });
    }
    if (path.startsWith("/v1/analytics/matrix-grid")) {
      const direction = query.get("direction") ?? "short";
      if (options.interceptMatrixGrid !== undefined) {
        const intercepted = options.interceptMatrixGrid(direction);
        if (intercepted !== undefined) return intercepted;
      }
      const items = direction === "long" ? options.longBulk ?? [] : options.shortBulk ?? [];
      return json({ items, ...selectionMetrics });
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
      const pathname = path.split("?")[0]!;
      const symbol = decodeURIComponent(pathname.split("/")[4] ?? "");
      const closeTimeUnix = Number(pathname.split("/")[5]);
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

it("computes matrices only after the button press and honours subsections", async () => {
  const calls = stubFetch({ shortBulk: fullShortGrid, longBulk: fullLongGrid });
  const { container } = render(<AnalyticsPage />);
  expect(container.querySelector('[data-slot="page-sidebar"]')).toBeInTheDocument();
  expect(container.querySelector('[data-slot="card"]')).toBeNull();
  expect(container.querySelector('[data-slot="segmented-list"]')).toBeNull();
  expect(await screen.findByText("#12 · Утренний прогон")).toBeInTheDocument();
  expect(screen.getByText("Снимок параметров запуска")).toBeInTheDocument();
  expect(screen.getByText("Рост цены (%)")).toBeInTheDocument();
  expect(screen.getByText("2.5")).toBeInTheDocument();
  expect(screen.getAllByRole("checkbox", { name: "Кластеризовать сетапы" }).length).toBeGreaterThan(0);
  expect(screen.getAllByLabelText("Зрелость сетапа в кластере: 0.5").length).toBeGreaterThan(0);
  expect(screen.getAllByLabelText("Разрыв кластера, ч")[0]).toHaveValue("8");
  expect(screen.getByRole("checkbox", { name: "Авторасчёт матриц" })).not.toBeChecked();
  expect(screen.queryByRole("button", { name: "Сохранить настройки" })).not.toBeInTheDocument();
  expect(screen.getAllByLabelText("Период")[0]).toHaveValue("all");
  expect(screen.getAllByLabelText("С даты")[0]).toHaveValue("2023-11-14");
  expect(screen.getAllByLabelText("По дату")[0]).toHaveValue("2023-11-15");
  expect(screen.getByRole("heading", { name: "Среднее матожидание сетапов" })).toBeInTheDocument();
  expect(screen.getByRole("heading", { name: "Матожидание по сетапам" })).toBeInTheDocument();
  expect(screen.queryByRole("heading", { name: "Кеш аналитики" })).not.toBeInTheDocument();
  expect(calls.some((call) => call.path.startsWith("/v1/analytics/matrix"))).toBe(false);

  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  await waitFor(() => expect(screen.getByText("Ячеек: 196 / 196")).toBeInTheDocument());
  expect(screen.getAllByText("0.17").length).toBeGreaterThan(0);

  expect(screen.getAllByLabelText("Красный при")[0]).toHaveValue("0.05");
  expect(screen.getAllByLabelText("Зелёный при")[0]).toHaveValue("0.2");
  const shortShift = screen.getAllByLabelText("Time-shift, мин")[0]!;
  fireEvent.change(shortShift, { target: { value: "7" } });

  await userEvent.click(screen.getByRole("tab", { name: "LONG" }));
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  await waitFor(() => expect(screen.getByText("Ячеек: 550 / 550")).toBeInTheDocument());
  expect(screen.getAllByText("-0.30").length).toBeGreaterThan(0);
  expect(screen.getAllByLabelText("Зелёный при")[1]).toHaveValue("0.5");
  await userEvent.click(screen.getByRole("tab", { name: "SHORT" }));
  expect(shortShift).toBeVisible(); expect(shortShift).toHaveValue("7");
  expect(screen.getByText("Ячеек: 196 / 196")).toBeVisible();
  expect(screen.getAllByRole("tabpanel")).toHaveLength(1);
  expect(calls.filter((call) => call.path === "/v1/analytics/matrix-grid")).toHaveLength(2);
}, 15_000);

it("fills the whole grid from a single batch request", async () => {
  const calls = stubFetch({ shortBulk: fullShortGrid });
  render(<AnalyticsPage />);
  expect(await screen.findByText("#12 · Утренний прогон")).toBeInTheDocument();
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  await waitFor(() => expect(screen.getByText("Ячеек: 196 / 196")).toBeInTheDocument(), { timeout: 15_000 });
  expect(calls.filter((call) => call.path === "/v1/analytics/matrix-grid")).toHaveLength(1);
  expect(calls.some((call) => call.path === "/v1/analytics/matrix-cell")).toBe(false);
}, 30_000);

it("stops the matrix batch run on demand and ignores the stale result", async () => {
  let gridCalls = 0;
  const firstRunResolve: { fn: ((response: Response) => void) | null } = { fn: null };
  stubFetch({
    shortBulk: fullShortGrid,
    interceptMatrixGrid: () => {
      gridCalls += 1;
      if (gridCalls === 1) {
        return new Promise<Response>((resolve) => { firstRunResolve.fn = resolve; });
      }
      return undefined;
    },
  });
  render(<AnalyticsPage />);
  await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  expect(await screen.findByRole("button", { name: "Остановить" })).toBeInTheDocument();
  expect(screen.getByText("Ячеек: 0 / 196")).toBeInTheDocument();

  await userEvent.click(screen.getByRole("button", { name: "Остановить" }));
  await waitFor(() => expect(screen.getAllByRole("button", { name: "Рассчитать" })).toHaveLength(2));

  firstRunResolve.fn?.(new Response(
    JSON.stringify({ items: fullShortGrid, ...selectionMetrics }),
    { status: 200 },
  ));
  await new Promise((resolve) => { setTimeout(resolve, 50); });
  expect(gridCalls).toBe(1);
  expect(screen.queryByText("0.17")).not.toBeInTheDocument();
}, 10_000);

it("disables maturity and sends no clustering params when the checkbox is off", async () => {
  const calls = stubFetch({ shortBulk: fullShortGrid });
  render(<AnalyticsPage />);
  await screen.findByText("#12 · Утренний прогон");
  expect(screen.getAllByLabelText("Зрелость сетапа в кластере: 0.5")[0]).toBeEnabled();
  await userEvent.click(screen.getAllByRole("checkbox", { name: "Кластеризовать сетапы" })[0]!);
  expect(screen.getAllByLabelText("Зрелость сетапа в кластере: 0.5")[0]).toBeDisabled();
  expect(screen.getAllByLabelText("Разрыв кластера, ч")[0]).toBeDisabled();
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  await waitFor(() => expect(screen.getByText("Ячеек: 196 / 196")).toBeInTheDocument());
  const bulkCalls = calls.filter((call) => call.path === "/v1/analytics/matrix-grid");
  expect(bulkCalls.length).toBeGreaterThan(0);
  expect(bulkCalls.every((call) => !call.search.includes("setupMaturity") && !call.search.includes("clusterGapHours"))).toBe(true);
  expect(screen.queryByText("После кластеризации: 42")).not.toBeInTheDocument();
  expect(screen.getAllByText("Сетапов: 42").length).toBe(2);
}, 10_000);

it("syncs the month selector with the date pair", async () => {
  const calls = stubFetch({ shortBulk: fullShortGrid });
  render(<AnalyticsPage />);
  await screen.findByText("#12 · Утренний прогон");
  await waitFor(() => expect(screen.getAllByText("2024-02")).toHaveLength(4));
  fireEvent.change(screen.getAllByLabelText("Период")[0]!, { target: { value: "2024-01" } });
  expect(screen.getAllByLabelText("С даты")[0]).toHaveValue("2024-01-01");
  expect(screen.getAllByLabelText("По дату")[0]).toHaveValue("2024-01-31");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  await waitFor(() => expect(screen.getByText("Ячеек: 196 / 196")).toBeInTheDocument());
  expect(calls.filter((call) => call.path === "/v1/analytics/matrix-grid").every((call) => call.search.includes("month=2024-01") && !call.search.includes("fromDay")));

  fireEvent.change(screen.getAllByLabelText("С даты")[0]!, { target: { value: "2024-01-10" } });
  expect(screen.getAllByLabelText("Период")[0]).toHaveValue("custom");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  await waitFor(() => expect(screen.getByText("Ячеек: 196 / 196")).toBeInTheDocument());
  const customCalls = calls.filter((call) => call.path === "/v1/analytics/matrix-grid" && call.search.includes("fromDay=2024-01-10"));
  expect(customCalls.length).toBeGreaterThan(0);
  expect(customCalls.every((call) => call.search.includes("toDay=2024-01-31") && !call.search.includes("month="))).toBe(true);
}, 10_000);

it("restores saved analytics settings and autosaves on commit", async () => {
  const calls = stubFetch({
    shortBulk: fullShortGrid,
    savedSettings: {
      setId: "run-12",
      clusterize: false,
      maturity: 0.7,
      clusterGapInput: "12",
      sections: {
        "short-matrix": { timeRange: { mode: "month", month: "2024-01", fromDay: null, toDay: null }, timeShiftInput: "5", considerPriceChange: true, priceChangeInput: "1.5", gradientMin: -0.1, gradientMax: 0.3 },
      },
    },
  });
  render(<AnalyticsPage />);
  expect(await screen.findByText("#12 · Утренний прогон")).toBeInTheDocument();
  expect(screen.getByLabelText("Зрелость сетапа в кластере: 0.7")).toBeDisabled();
  expect(screen.getByLabelText("Разрыв кластера, ч")).toHaveValue("12");
  expect(screen.getByRole("checkbox", { name: "Кластеризовать сетапы" })).not.toBeChecked();
  expect(screen.getAllByLabelText("Период")[0]).toHaveValue("2024-01");
  expect(screen.getAllByLabelText("С даты")[0]).toHaveValue("2024-01-01");
  expect(screen.getAllByLabelText("Time-shift, мин")[0]).toHaveValue("5");
  expect(screen.getAllByLabelText("Смещение цены, %")[0]).toHaveValue("1.5");
  expect(screen.getAllByLabelText("Красный при")[0]).toHaveValue("-0.1");
  expect(screen.getAllByLabelText("Период")[1]).toHaveValue("all");
  fireEvent.change(screen.getByLabelText("Разрыв кластера, ч"), { target: { value: "20" } });
  expect(calls.filter((call) => call.method === "PUT" && call.path === "/v1/settings/web-analytics")).toHaveLength(0);
  fireEvent.blur(screen.getByLabelText("Разрыв кластера, ч"));
  await waitFor(() => expect(calls.filter((call) => call.method === "PUT" && call.path === "/v1/settings/web-analytics")).toHaveLength(1), { timeout: 3000 });
  const putCall = calls.find((call) => call.method === "PUT" && call.path === "/v1/settings/web-analytics")!;
  const sent = JSON.parse(putCall.body!) as Record<string, unknown>;
  expect(sent.setId).toBe("run-12");
  expect(sent.clusterize).toBe(false);
  expect(sent.maturity).toBe(0.7);
  expect(sent.clusterGapInput).toBe("20");
  expect(sent.autoCalc).toBe(false);
  const sections = sent.sections as Record<string, Record<string, unknown>>;
  expect(sections["short-matrix"]).toMatchObject({ timeShiftInput: "5", considerPriceChange: true, priceChangeInput: "1.5", gradientMin: -0.1, gradientMax: 0.3 });
  expect(sections["short-trades"]).toMatchObject({ tpInput: "4", slInput: "4" });
}, 10_000);

it("autoruns matrices only for matrix-relevant changes", async () => {
  const calls = stubFetch({
    shortBulk: fullShortGrid,
    longBulk: fullLongGrid,
    savedSettings: { autoCalc: true },
  });
  render(<AnalyticsPage />);
  expect(await screen.findByText("#12 · Утренний прогон")).toBeInTheDocument();
  await waitFor(() => expect(screen.getByText("Ячеек: 196 / 196")).toBeInTheDocument(), { timeout: 15_000 });
  await waitFor(() => expect(screen.getByText("Ячеек: 550 / 550")).toBeInTheDocument(), { timeout: 15_000 });
  const matrixCallsAfterEntry = calls.filter((call) => call.path.startsWith("/v1/analytics/matrix")).length;
  fireEvent.change(screen.getAllByLabelText("TP, %")[0]!, { target: { value: "5" } });
  fireEvent.blur(screen.getAllByLabelText("TP, %")[0]!);
  await waitFor(() => expect(calls.filter((call) => call.method === "PUT" && call.path === "/v1/settings/web-analytics")).toHaveLength(1), { timeout: 3000 });
  expect(calls.filter((call) => call.path.startsWith("/v1/analytics/matrix")).length).toBe(matrixCallsAfterEntry);
  expect(calls.every((call) => !call.path.startsWith("/v1/analytics/setups") && !call.path.startsWith("/v1/analytics/trades"))).toBe(true);
  await waitFor(() => expect(screen.getAllByText("2024-02")).toHaveLength(4), { timeout: 5000 });
  fireEvent.change(screen.getAllByLabelText("Период")[0]!, { target: { value: "2024-01" } });
  await waitFor(() => expect(calls.filter((call) => call.path.startsWith("/v1/analytics/matrix")).length).toBeGreaterThan(matrixCallsAfterEntry), { timeout: 5000 });
}, 30_000);

it("auto-calculates matrices on entry when enabled", async () => {
  const calls = stubFetch({
    shortBulk: fullShortGrid,
    longBulk: fullLongGrid,
    savedSettings: { autoCalc: true },
  });
  render(<AnalyticsPage />);
  expect(await screen.findByText("#12 · Утренний прогон")).toBeInTheDocument();
  expect(screen.getByRole("checkbox", { name: "Авторасчёт матриц" })).toBeChecked();
  await waitFor(() => expect(screen.getByText("Ячеек: 196 / 196")).toBeInTheDocument(), { timeout: 15_000 });
  await waitFor(() => expect(screen.getByText("Ячеек: 550 / 550")).toBeInTheDocument(), { timeout: 15_000 });
  expect(calls.some((call) => call.path === "/v1/analytics/matrix-grid")).toBe(true);
  expect(calls.every((call) => !call.path.startsWith("/v1/analytics/setups") && !call.path.startsWith("/v1/analytics/trades"))).toBe(true);
}, 30_000);

it("runs the SHORT trade list cache-first and builds the daily expectancy chart", async () => {
  stubFetch();
  render(<AnalyticsPage />);
  await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  expect(await screen.findByText("BTCUSDT")).toBeInTheDocument();
  expect(await screen.findByText("ETHUSDT")).toBeInTheDocument();
  await waitFor(() => expect(screen.getByText("Win rate: 50.0%")).toBeInTheDocument());
  expect(screen.getByText("Матожидание: +0.00R по 2 закрытым")).toBeInTheDocument();
  const chart = screen.getByRole("img", { name: "Дневное матожидание в рисках" });
  expect(chart).toBeInTheDocument();
  expect(chart.querySelectorAll("title").length).toBeGreaterThan(0);
}, 10_000);

it("keeps the cache summary in the third subsection and clears it", async () => {
  const calls = stubFetch();
  render(<AnalyticsPage />);
  await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getByRole("tab", { name: "Кеш" }));
  expect(await screen.findByText("Всего: 8")).toBeInTheDocument();
  expect(screen.getByText("analytics_matrix_cache")).toBeInTheDocument();
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
  await screen.findByText("#12 · Утренний прогон");
  fireEvent.change(screen.getAllByLabelText("С даты")[1]!, { target: { value: "2124-01-01" } });
  expect(screen.getAllByLabelText("Период")[1]).toHaveValue("custom");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  expect(await screen.findByText(/Сделок: 2 \/ 2/)).toBeInTheDocument();
  expect(screen.queryByText("BTCUSDT")).not.toBeInTheDocument();
}, 10_000);

it("shows deposit growth per trade and a full-width daily chart", async () => {
  stubFetch();
  render(<AnalyticsPage />);
  await screen.findByText("#12 · Утренний прогон");
  expect(screen.getAllByLabelText("Риск на сделку, %")[0]).toHaveValue("1");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await waitFor(() => expect(screen.getByText("Сделок: 2 / 2")).toBeInTheDocument());
  expect(screen.getByText("Депозит, х")).toBeInTheDocument();
  expect(screen.getByText("1.01")).toBeInTheDocument();
  expect(screen.getByText("1.00")).toBeInTheDocument();
  const chart = screen.getByRole("img", { name: "Дневное матожидание в рисках" });
  expect(chart).toHaveClass("w-full");
  expect(chart).not.toHaveClass("max-w-3xl");
  fireEvent.change(screen.getAllByLabelText("Риск на сделку, %")[0]!, { target: { value: "0" } });
  await screen.findByText("Риск на сделку должен быть числом больше 0 и не больше 100");
}, 15_000);
