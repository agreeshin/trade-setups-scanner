import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { AnalyticsPage } from "./analytics-page";
import { SharedForecastControls } from "../components/shared-forecast-controls";
import type { MatrixCellRef, TradeRow, TrendInput, TrendSummary, TrendProjectionRequest } from "../lib/analytics-api";
import { applyInterfaceSettings, zonedDayStart } from "../lib/datetime-format";
import type { ForecastCurveRequest, ForecastCurveResponse } from "../lib/forecast-api";
import { forecastFixture, withDisplayCurve } from "../../test/forecast-fixture";

afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals(); applyInterfaceSettings({ timezone: "auto" }); });
beforeEach(() => { applyInterfaceSettings({ timezone: "UTC" }); });

const setFixture = {
  setId: "run-12", kind: "run", numericId: 12, label: "#12 · Утренний прогон",
  algorithmId: "price_volume_spike", algorithmVersion: "1.1.1", params: { priceGrowthPct: "2.5", windowMinutes: 30, volumeMultiplier: "3" },
  exchange: "bybit", category: "linear", symbol: "BTCUSDT", setupCount: 42, fromUnix: 1_700_000_000, toUnix: 1_700_086_400, createdAt: "2024-01-15T00:00:00Z",
  firstSetupAt: 1_700_000_060 as number | null, lastSetupAt: 1_700_003_660 as number | null,
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
  shortBulk?: MatrixCellRef[];
  longBulk?: MatrixCellRef[];
  interceptMatrixGrid?: (direction: string) => Promise<Response> | undefined;
  savedSettings?: Record<string, unknown>;
  tradeRows?: TradeRow[];
  trend?: TrendSummary | null;
  trendError?: boolean;
  interceptTrend?: (signal: AbortSignal | undefined) => Promise<Response>;
  interceptTradeRows?: () => Promise<Response> | undefined;
  interceptTradeRow?: (setup: { symbol: string; closeTimeUnix: number }, signal: AbortSignal | undefined) => Promise<Response>;
  forecast?: (request: ForecastCurveRequest, signal: AbortSignal | undefined) => Promise<Response>;
}

// Transport fixture only; spline/SE arithmetic is tested in Rust. This weighted
// line supplies known values for the legacy calendar/viewport rendering cases.
function fixtureTrend(points: TrendInput[]): TrendSummary | null {
  const finite = points.filter((p): p is TrendInput & { value: number } => p.value !== null && p.weight > 0);
  if (finite.length < 2) return null;
  const weight = finite.reduce((sum, p) => sum + p.weight, 0);
  const meanX = finite.reduce((sum, p) => sum + p.day * p.weight, 0) / weight;
  const meanY = finite.reduce((sum, p) => sum + p.value * p.weight, 0) / weight;
  const slope = finite.reduce((sum, p) => sum + p.weight * (p.day - meanX) * (p.value - meanY), 0) / finite.reduce((sum, p) => sum + p.weight * (p.day - meanX) ** 2, 0);
  const start = { day: finite[0]!.day, value: meanY + slope * (finite[0]!.day - meanX), se: finite.length > 2 ? 0.2 : null };
  const end = { day: finite[finite.length - 1]!.day, value: meanY + slope * (finite[finite.length - 1]!.day - meanX), se: start.se };
  return { method: "linear", start, end, endpointSe: end.se, curve: [start, end] };
}

function stubFetch(options: StubOptions = {}) {
  const calls: { method: string; path: string; search: string; body?: string }[] = [];
  vi.stubGlobal("fetch", vi.fn((url: string, init?: RequestInit) => {
    const path = url.replace(/^https?:\/\/[^/]+/, "");
    const query = new URLSearchParams(path.split("?")[1] ?? "");
    const method = init?.method ?? "GET";
    calls.push({ method, path: path.split("?")[0]!, search: path.split("?")[1] ?? "", ...(typeof init?.body === "string" ? { body: init.body } : {}) });
    const json = (body: unknown, status = 200) => Promise.resolve(new Response(JSON.stringify(body), { status }));
    if (path === "/v1/analytics/forecast") {
      const request = JSON.parse(init?.body as string) as ForecastCurveRequest;
      return options.forecast ? options.forecast(request, init?.signal ?? undefined) : json(forecastFixture(request));
    }
    if (path === "/v1/analytics/trend") {
      if (options.interceptTrend) return options.interceptTrend(init?.signal ?? undefined);
      if (options.trendError) return json({ error: "trend unavailable" }, 503);
      const request = JSON.parse(typeof init?.body === "string" ? init.body : "{}") as { points: TrendInput[]; projection?: TrendProjectionRequest };
      const trend = options.trend === undefined ? fixtureTrend(request.points) : options.trend;
      if (!request.projection) return json({ trend });
      const projection = request.projection;
      // Transport fixture only; exact linear/spline/M-F-B parity belongs to Rust.
      const decision = (forecast: number | null) => projection.trendMode === "mean" ? projection.meanR : forecast === null ? null
        : projection.trendMode === "forecast" ? forecast : projection.meanR === null ? null
          : (1 - projection.trendBlendLambda!) * projection.meanR + projection.trendBlendLambda! * forecast;
      const decisionR = decision(trend?.end.value ?? null);
      const coordinates = trend?.curve ?? (projection.trendMode === "mean" && request.points.length > 0
        ? [request.points[0]!, ...(request.points.length > 1 ? [request.points.at(-1)!] : [])] : []);
      return json({ trend, projection: { ...projection, forecastR: trend?.end.value ?? null, decisionR,
        curve: decisionR === null ? [] : coordinates.map((point) => ({ day: point.day, value: decision(point.value) })) } });
    }
    if (path.startsWith("/v1/settings/web-analytics")) {
      if (method === "PUT") return json({ settings: typeof init?.body === "string" ? (JSON.parse(init.body) as unknown) : {} });
      return json({ settings: options.savedSettings ?? { trendMode: "mean" } });
    }
    if (path.startsWith("/v1/analytics/sets")) return json({ sets: options.sets ?? [setFixture] });
    if (path.startsWith("/v1/analytics/months")) return json({ months: ["2024-01", "2024-02"] });
    if (path.startsWith("/v1/analytics/setups")) {
      return json({ setups: options.tradeRows?.map((row) => row.setup) ?? [setupA, setupB], ...selectionMetrics });
    }
    if (path.startsWith("/v1/analytics/matrix-grid")) {
      const direction = query.get("direction") ?? "short";
      if (options.interceptMatrixGrid !== undefined) {
        const intercepted = options.interceptMatrixGrid(direction);
        if (intercepted !== undefined) return intercepted;
      }
      const items = direction === "long" ? options.longBulk ?? [] : options.shortBulk ?? [];
      const recipe = query.get("forecast");
      return json({ items: recipe ? items.map((cell) => ({ ...cell, forecast: JSON.parse(recipe) as unknown, observationCutoff: Number(query.get("observationCutoff")), meanR: 0.8, meanRiskExpectancy: 0.8, forecastR: 0.4, decisionR: cell.riskExpectancy })) : items, ...selectionMetrics });
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
      if (options.interceptTradeRow) return options.interceptTradeRow({ symbol, closeTimeUnix }, init?.signal ?? undefined);
      const fixture = options.tradeRows?.find((row) => row.setup.symbol === symbol && row.setup.closeTimeUnix === closeTimeUnix);
      if (fixture !== undefined) return json({ ...fixture, cached: false });
      const status = symbol === "BTCUSDT" ? "tp" : "sl";
      return json({
        setup: { ...setupA, symbol, closeTimeUnix },
        direction: "short", tpPct: 4, slPct: 4, status,
        entryPrice: 100, tpPrice: status === "tp" ? 96 : null, slPrice: status === "sl" ? 104 : null,
        closeTimeUnix: closeTimeUnix + 600, riskReturn: status === "tp" ? 1 : -1, cached: false,
      });
    }
    if (path.startsWith("/v1/analytics/trades")) {
      const intercepted = options.interceptTradeRows?.();
      if (intercepted !== undefined) return intercepted;
      if (options.tradeRows) return json({ items: options.tradeRows, cachedCount: options.tradeRows.length });
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

it("uses the exact former axis defaults/dimensions without a write-back migration", async () => {
  const calls = stubFetch();
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  expect(screen.getAllByLabelText("Уровни TP, %").map((input) => (input as HTMLInputElement).value)).toEqual([shortLevels.join(", "), longTpLevels.join(", ")]);
  expect(screen.getAllByLabelText("Уровни SL, %").map((input) => (input as HTMLInputElement).value)).toEqual([shortLevels.join(", "), longSlLevels.join(", ")]);
  const tables = screen.getAllByRole("table", { hidden: true });
  expect(within(tables[0]!).getAllByRole("row", { hidden: true })).toHaveLength(15);
  expect(within(tables[0]!).getAllByRole("cell", { hidden: true })).toHaveLength(196);
  expect(within(tables[1]!).getAllByRole("cell", { hidden: true })).toHaveLength(550);
  expect(calls.some((call) => call.method === "PUT")).toBe(false);
  fireEvent.blur(screen.getByLabelText("Разрыв кластера, ч"));
  await waitFor(() => expect(calls.some((call) => call.method === "PUT")).toBe(true), { timeout: 3000 });
  const saved = JSON.parse(calls.find((call) => call.method === "PUT")!.body!) as { sections: Record<string, Record<string, unknown>> };
  expect(saved.sections["short-matrix"]).not.toHaveProperty("tpAxisInput");
  expect(saved.sections["long-matrix"]).not.toHaveProperty("slAxisInput");
});

it("persists custom axes in entered order, renders that actual matrix shape regardless of response order, and preserves shared settings", async () => {
  const cells = [8, 4, 6].flatMap((tp) => [2, 1].map((sl) => matrixCell(tp, sl, tp / sl))).reverse();
  const calls = stubFetch({ shortBulk: cells, savedSettings: { trendMode: "mean", unrelated: { enabled: true }, sections: {
    "short-matrix": { tpAxisInput: "8, 4, 6", slAxisInput: "2, 1", custom: "keep" }, "short-trades": { tpInput: "9", slInput: "3" },
  } } });
  render(<><AnalyticsPage /><SharedForecastControls /></>); await screen.findByText("#12 · Утренний прогон");
  const panel = within(screen.getByRole("tabpanel"));
  const table = panel.getByRole("table");
  const rows = within(table).getAllByRole("row");
  expect(within(rows[0]!).getAllByRole("columnheader").map((cell) => cell.textContent)).toEqual(["TP \\ SL", "2%", "1%"]);
  expect(rows.slice(1).map((row) => row.firstElementChild?.textContent)).toEqual(["8%", "4%", "6%"]);
  expect(within(table).getAllByRole("cell")).toHaveLength(6);
  expect(panel.getByLabelText("TP, %")).toHaveValue("9"); expect(panel.getByLabelText("SL, %")).toHaveValue("3");
  await userEvent.click(panel.getAllByRole("button", { name: "Рассчитать" })[0]!);
  await panel.findByText("Ячеек: 6 / 6");
  const query = new URLSearchParams(calls.find((call) => call.path === "/v1/analytics/matrix-grid")!.search);
  expect(query.get("tpPct")).toBe("8,4,6"); expect(query.get("slPct")).toBe("2,1");
  expect(within(rows[1]!).getAllByRole("cell").map((cell) => cell.textContent)).toEqual(["4.00", "8.00"]);
  fireEvent.change(panel.getByLabelText("Уровни TP, %"), { target: { value: "6, 8" } });
  fireEvent.blur(panel.getByLabelText("Уровни TP, %"));
  await waitFor(() => expect(calls.filter((call) => call.method === "PUT")).toHaveLength(1), { timeout: 3000 });
  expect(calls.filter((call) => call.path === "/v1/analytics/matrix-grid")).toHaveLength(1);
  const shared = within(screen.getByRole("region", { name: "Общий прогноз Analytics и Сетапов" }));
  fireEvent.click(shared.getByRole("button", { name: "Прогноз" }));
  await waitFor(() => expect(calls.filter((call) => call.method === "PUT")).toHaveLength(2), { timeout: 3000 });
  const saved = JSON.parse(calls.filter((call) => call.method === "PUT").at(-1)!.body!) as unknown;
  expect(saved).toMatchObject({ unrelated: { enabled: true }, trendMode: "forecast", sections: {
    "short-matrix": { tpAxisInput: "6, 8", slAxisInput: "2, 1", custom: "keep" }, "short-trades": { tpInput: "9", slInput: "3" },
  } });
  expect(calls.some((call) => call.path === "/v1/analytics/setups" || call.path.includes("/trades"))).toBe(false);
});

it.each(["", "1,1.0", "4,", "0", "100.1", "Infinity", "0x10", Array.from({ length: 65 }, (_, index) => index + 1).join(","), null])("keeps an invalid persisted axis %s visible and suppresses requests", async (invalid) => {
  const calls = stubFetch({ savedSettings: { sections: { "short-matrix": { tpAxisInput: invalid, slAxisInput: "1" } } } });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  const panel = within(screen.getByRole("tabpanel"));
  const input = panel.getByLabelText("Уровни TP, %");
  expect(input).toHaveValue(invalid ?? "Некорректное сохранённое значение: null");
  expect(input).toHaveAttribute("aria-invalid", "true");
  expect(panel.getByRole("alert")).toHaveTextContent("TP:");
  await userEvent.click(panel.getAllByRole("button", { name: "Рассчитать" })[0]!);
  expect(calls.some((call) => call.path === "/v1/analytics/matrix-grid")).toBe(false);
});

it("fences a pending matrix response after an axis-only edit and autoCalc refreshes only matrices", async () => {
  let release: ((response: Response) => void) | undefined;
  let attempts = 0;
  const calls = stubFetch({ savedSettings: { autoCalc: true, sections: { "short-matrix": { tpAxisInput: "8", slAxisInput: "2" }, "long-matrix": { tpAxisInput: "1", slAxisInput: "1" } } },
    shortBulk: [matrixCell(4, 2, 0.42)], interceptMatrixGrid: (direction) => direction === "short" && ++attempts === 1 ? new Promise<Response>((resolve) => { release = resolve; }) : undefined });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await waitFor(() => expect(release).toBeDefined());
  const panel = within(screen.getByRole("tabpanel"));
  fireEvent.change(panel.getByLabelText("Уровни TP, %"), { target: { value: "4" } });
  fireEvent.blur(panel.getByLabelText("Уровни TP, %"));
  act(() => { release!(new Response(JSON.stringify({ items: [matrixCell(8, 2, 9.99)], ...selectionMetrics }))); });
  expect(panel.queryByText("9.99")).not.toBeInTheDocument();
  expect(await panel.findByText("0.42", {}, { timeout: 3000 })).toBeVisible();
  const short = calls.filter((call) => call.path === "/v1/analytics/matrix-grid" && new URLSearchParams(call.search).get("direction") === "short");
  expect(short.map((call) => new URLSearchParams(call.search).get("tpPct"))).toEqual(["8", "4"]);
  expect(calls.some((call) => call.path === "/v1/analytics/setups" || call.path.includes("/trades"))).toBe(false);
});

it("keeps accessible disclosure semantics, explicitly shows its triangle and centers heading/help groups", async () => {
  stubFetch({ savedSettings: { trendMode: "forecast", forecast: probabilityRecipe } });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  for (const name of ["Матожидание и модель прогноза", "Прогноз матожидания", "Расчёт ячейки"]) {
    expect(screen.getByRole("heading", { name }).parentElement).toHaveClass("items-center", "justify-center", "text-center");
  }
  const trigger = screen.getByRole("button", { name: "Расширенные настройки модели" });
  expect(trigger).toHaveAttribute("aria-expanded", "false");
  expect(within(trigger).getByTestId("model-disclosure-triangle")).toHaveAttribute("aria-hidden", "true");
  expect(screen.queryByLabelText("Сила prior κ")).not.toBeInTheDocument();
  trigger.focus(); await userEvent.keyboard("{Enter}");
  expect(trigger).toHaveAttribute("aria-expanded", "true");
  expect(screen.getByLabelText("Сила prior κ")).toBeVisible();
  expect(document.getElementById(trigger.getAttribute("aria-controls")!)).toContainElement(screen.getByLabelText("Сила prior κ"));
  await userEvent.keyboard(" "); expect(trigger).toHaveAttribute("aria-expanded", "false");
});

it.each([false, true])("formats actual first-last signal extrema (single=$single) without refetching or mutating exact ranges", async (single) => {
  const first = Date.parse("2026-01-01T01:00:00Z") / 1000;
  const last = single ? first : Date.parse("2026-01-02T01:00:00Z") / 1000;
  const fromUnix = first - 60, toUnix = last + 60;
  const calls = stubFetch({ sets: [{ ...setFixture, firstSetupAt: first, lastSetupAt: last }], savedSettings: { sections: { "short-matrix": { timeRange: { mode: "custom", fromUnix, toUnix } } } } });
  applyInterfaceSettings({ timezone: "America/New_York" });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  const tag = screen.getByLabelText("Даты первого и последнего сетапа");
  expect(tag).toHaveTextContent(single ? "31.12.2025 — 31.12.2025" : "31.12.2025 — 01.01.2026");
  const before = calls.length;
  act(() => { applyInterfaceSettings({ timezone: "Asia/Kathmandu", dateOrder: "ymd", dateSeparator: "/" }); });
  expect(tag).toHaveTextContent(single ? "2026/01/01 — 2026/01/01" : "2026/01/01 — 2026/01/02");
  expect(calls).toHaveLength(before);
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  const query = new URLSearchParams(calls.find((call) => call.path === "/v1/analytics/matrix-grid")!.search);
  expect(query.get("fromUnix")).toBe(String(fromUnix)); expect(query.get("toUnix")).toBe(String(toUnix));
  expect(calls.filter((call) => call.path === "/v1/analytics/sets")).toHaveLength(1);
});

it("does not invent first/last dates for an empty set despite nonempty execution bounds", async () => {
  const calls = stubFetch({ sets: [{ ...setFixture, setupCount: 0, firstSetupAt: null, lastSetupAt: null }] });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  expect(screen.getByLabelText("Даты первого и последнего сетапа")).toHaveTextContent("Даты сетапов: —");
  expect(calls.filter((call) => call.path === "/v1/analytics/sets")).toHaveLength(1);
});

it("shares exact partial-day minute bounds across matrix, cell and forecast, includes start and excludes end without hidden calculation", async () => {
  applyInterfaceSettings({ timezone: "America/New_York", time24: false });
  const fromUnix = Date.parse("2026-03-08T06:30:00Z") / 1000;
  const toUnix = Date.parse("2026-03-08T07:30:00Z") / 1000;
  const rows = [utcTrade("2026-03-08T06:29:00Z", -1, 0), utcTrade("2026-03-08T06:30:00Z", 1, 1),
    utcTrade("2026-03-08T07:29:00Z", 1, 2), utcTrade("2026-03-08T07:30:00Z", -1, 3)];
  const calls = stubFetch({ tradeRows: rows, savedSettings: { trendMode: "mean", forecast: { method: "decayed_probability", version: 1, halfLifeDays: 14, kappa: 4, priorPolicy: "gross_break_even" },
    sections: { "short-matrix": { timeRange: { mode: "custom", fromUnix: fromUnix + 37, toUnix: toUnix + 59 } } } } });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  expect(screen.getAllByLabelText("С даты")[0]).toHaveValue("2026-03-08T01:30");
  expect(screen.getAllByLabelText("По дату")[0]).toHaveValue("2026-03-08T03:30");
  expect(screen.getAllByLabelText("С даты")[0]!.closest('[data-slot="date-control"]')).toHaveTextContent("01:30 AM");
  expect(screen.getAllByLabelText("По дату")[0]!.closest('[data-slot="date-control"]')).toHaveTextContent("03:30 AM");
  expect(calls.some((call) => call.path.includes("/trades") || call.path.includes("/matrix-grid"))).toBe(false);
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await screen.findByText("Матожидание: +1.00R по 2 закрытым");
  expect(screen.queryByText("UTC0USDT")).not.toBeInTheDocument();
  expect(screen.getByText("UTC1USDT")).toBeInTheDocument();
  expect(screen.queryByText("UTC3USDT")).not.toBeInTheDocument();
  const scoped = calls.filter((call) => ["/v1/analytics/matrix-grid", "/v1/analytics/setups", "/v1/analytics/trades"].includes(call.path));
  expect(scoped).toHaveLength(3);
  for (const call of scoped) {
    const query = new URLSearchParams(call.search);
    expect(query.get("fromUnix")).toBe(String(fromUnix)); expect(query.get("toUnix")).toBe(String(toUnix));
    expect(query.has("fromDay") || query.has("month")).toBe(false);
  }
  await waitFor(() => expect(calls.some((call) => call.path === "/v1/analytics/forecast")).toBe(true));
  const graph = JSON.parse(calls.find((call) => call.path === "/v1/analytics/forecast")!.body!) as ForecastCurveRequest;
  expect(graph.fromUnix).toBe(fromUnix); expect(graph.toUnix).toBe(toUnix);
  const count = scoped.length;
  fireEvent.change(screen.getAllByLabelText("С даты")[0]!, { target: { value: "2026-03-08T01:31" } });
  fireEvent.blur(screen.getAllByLabelText("С даты")[0]!);
  expect(screen.getAllByLabelText("Период")[0]).toHaveValue("custom");
  await waitFor(() => expect(calls.some((call) => call.method === "PUT")).toBe(true), { timeout: 3000 });
  expect(calls.filter((call) => ["/v1/analytics/matrix-grid", "/v1/analytics/setups", "/v1/analytics/trades"].includes(call.path))).toHaveLength(count);
  const saved = JSON.parse(calls.find((call) => call.method === "PUT")!.body!) as { sections: Record<string, { timeRange?: unknown }> };
  expect(saved.sections["short-matrix"]!.timeRange).toEqual({ mode: "custom", month: "custom", fromUnix: fromUnix + 60, toUnix });
  expect(screen.queryByText("UTC1USDT")).not.toBeInTheDocument();
});

it.each([
  { mode: "custom", month: "custom", fromDay: "2026-03-08", toDay: "2026-03-08" },
  { mode: "month", month: "2026-03", fromDay: null, toDay: null },
])("preserves legacy $mode fields on unrelated saves and persists exact bounds only after a range edit", async (legacy) => {
  applyInterfaceSettings({ timezone: "America/New_York" });
  const fromUnix = Date.parse(legacy.mode === "month" ? "2026-03-01T00:00:00Z" : "2026-03-08T00:00:00Z") / 1000;
  const toUnix = Date.parse(legacy.mode === "month" ? "2026-04-01T00:00:00Z" : "2026-03-09T00:00:00Z") / 1000;
  const rows = [utcTrade(new Date((fromUnix + 60) * 1000).toISOString(), 1, 0), utcTrade(new Date((fromUnix + 120) * 1000).toISOString(), -1, 1)];
  const calls = stubFetch({ tradeRows: rows, savedSettings: { forecast: { method: "decayed_probability", version: 1, halfLifeDays: 14, kappa: 4, priorPolicy: "gross_break_even" },
    sections: { "short-matrix": { timeRange: legacy }, "long-matrix": { timeRange: legacy } } } });
  render(<><AnalyticsPage /><SharedForecastControls /></>); await screen.findByText("#12 · Утренний прогон");
  expect(screen.getAllByLabelText("С даты")[0]).toHaveValue(legacy.mode === "month" ? "2026-02-28T19:00" : "2026-03-07T19:00");
  expect(screen.getAllByLabelText("По дату")[0]).toHaveValue(legacy.mode === "month" ? "2026-03-31T20:00" : "2026-03-08T20:00");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  const first = new URLSearchParams(calls.find((call) => call.path === "/v1/analytics/matrix-grid")!.search);
  expect(first.get("fromUnix")).toBe(String(fromUnix)); expect(first.get("toUnix")).toBe(String(toUnix));
  act(() => { applyInterfaceSettings({ timezone: "Pacific/Auckland", time24: false }); });
  const localFrom = legacy.mode === "month" ? "2026-03-01T13:00" : "2026-03-08T13:00";
  expect(screen.getAllByLabelText("С даты")[0]).toHaveValue(localFrom);
  expect(screen.getAllByLabelText("По дату")[0]).toHaveValue(legacy.mode === "month" ? "2026-04-01T13:00" : "2026-03-09T13:00");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  const second = new URLSearchParams(calls.filter((call) => call.path === "/v1/analytics/matrix-grid").at(-1)!.search);
  expect(second.toString()).toBe(first.toString());
  await act(async () => { await new Promise((resolve) => { setTimeout(resolve, 850); }); });
  expect(calls.some((call) => call.method === "PUT" || call.path.includes("/trades") || call.path === "/v1/analytics/setups")).toBe(false);

  const puts = () => calls.filter((call) => call.method === "PUT");
  const savedRange = (index: number, direction = "short") => (JSON.parse(puts()[index]!.body!) as { sections: Record<string, { timeRange: unknown }> }).sections[`${direction}-matrix`]!.timeRange;
  const shared = within(screen.getByRole("region", { name: "Общий прогноз Analytics и Сетапов" }));
  fireEvent.click(shared.getByRole("button", { name: "Прогноз" }));
  await waitFor(() => expect(puts()).toHaveLength(1), { timeout: 3000 });
  expect(savedRange(0)).toEqual(legacy); expect(savedRange(0, "long")).toEqual(legacy);
  fireEvent.change(screen.getAllByLabelText("Time-shift, мин")[0]!, { target: { value: "7" } });
  fireEvent.blur(screen.getAllByLabelText("Time-shift, мин")[0]!);
  await waitFor(() => expect(puts()).toHaveLength(2), { timeout: 3000 });
  expect(savedRange(1)).toEqual(legacy); expect(savedRange(1, "long")).toEqual(legacy);
  expect(JSON.parse(puts()[1]!.body!) as unknown).toMatchObject({ trendMode: "forecast", sections: { "short-matrix": { timeShiftInput: "7" } } });

  fireEvent.change(screen.getAllByLabelText("С даты")[0]!, { target: { value: localFrom.replace(":00", ":01") } });
  fireEvent.blur(screen.getAllByLabelText("С даты")[0]!);
  expect(screen.getAllByLabelText("Период")[0]).toHaveValue("custom");
  await waitFor(() => expect(puts()).toHaveLength(3), { timeout: 3000 });
  expect(savedRange(2)).toEqual({ mode: "custom", month: "custom", fromUnix: fromUnix + 60, toUnix });
  expect(savedRange(2, "long")).toEqual(legacy);
  expect(calls.filter((call) => call.path === "/v1/analytics/matrix-grid")).toHaveLength(2);
  expect(calls.some((call) => call.path.includes("/trades") || call.path === "/v1/analytics/setups" || call.path === "/v1/analytics/forecast")).toBe(false);
  const beforeCalculation = calls.length;
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await waitFor(() => expect(calls.some((call) => call.path === "/v1/analytics/forecast")).toBe(true));
  const scoped = calls.slice(beforeCalculation).filter((call) => ["/v1/analytics/matrix-grid", "/v1/analytics/setups", "/v1/analytics/trades"].includes(call.path));
  expect(scoped).toHaveLength(3);
  for (const call of scoped) {
    const query = new URLSearchParams(call.search);
    expect(query.get("fromUnix")).toBe(String(fromUnix + 60)); expect(query.get("toUnix")).toBe(String(toUnix));
    for (const field of ["month", "fromDay", "toDay"]) expect(query.has(field)).toBe(false);
  }
  const graph = JSON.parse(calls.find((call) => call.path === "/v1/analytics/forecast")!.body!) as ForecastCurveRequest;
  expect(graph.fromUnix).toBe(fromUnix + 60); expect(graph.toUnix).toBe(toUnix);
}, 15_000);

it("fences a late matrix and list response after a same-day one-minute scope edit without starting another request", async () => {
  const pending: { matrix?: (response: Response) => void; trades?: (response: Response) => void } = {};
  const calls = stubFetch({ interceptMatrixGrid: () => new Promise<Response>((resolve) => { pending.matrix = resolve; }),
    interceptTradeRows: () => new Promise<Response>((resolve) => { pending.trades = resolve; }) });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!); // Matrix is now “Остановить”; the remaining calculate button is the cell.
  await waitFor(() => expect(pending.trades).toBeDefined());
  fireEvent.change(screen.getAllByLabelText("С даты")[0]!, { target: { value: "2023-11-14T00:01" } });
  await act(async () => {
    pending.matrix!(new Response(JSON.stringify({ items: fullShortGrid, ...selectionMetrics })));
    pending.trades!(new Response(JSON.stringify({ items: [{ setup: setupA, status: "tp", riskReturn: 1 }], cachedCount: 1 })));
    await Promise.resolve();
  });
  expect(screen.queryByText("0.17")).not.toBeInTheDocument(); expect(screen.queryByText("BTCUSDT")).not.toBeInTheDocument();
  expect(calls.filter((call) => call.path === "/v1/analytics/matrix-grid")).toHaveLength(1);
  expect(calls.filter((call) => call.path === "/v1/analytics/trades")).toHaveLength(1);
  expect(calls.some((call) => call.path.startsWith("/v1/analytics/trades/"))).toBe(false);
});

it("keeps the published matrix and list for exact range B when range A resolves last after a one-minute edit", async () => {
  const fromUnix = Date.parse("2026-01-04T11:30:00Z") / 1000;
  const toUnix = Date.parse("2026-01-04T12:30:00Z") / 1000;
  const current = utcTrade("2026-01-04T12:00:00Z", 1, 0);
  const stale = { ...current, status: "sl", riskReturn: -1 };
  const pending: { matrix?: (response: Response) => void; trades?: (response: Response) => void } = {};
  let matrixAttempts = 0; let tradeAttempts = 0;
  const calls = stubFetch({ tradeRows: [current], shortBulk: [matrixCell(4, 4, 0.42)], savedSettings: { trendMode: "mean",
    sections: { "short-matrix": { timeRange: { mode: "custom", fromUnix, toUnix } } } },
    interceptMatrixGrid: () => ++matrixAttempts === 1 ? new Promise<Response>((resolve) => { pending.matrix = resolve; }) : undefined,
    interceptTradeRows: () => ++tradeAttempts === 1 ? new Promise<Response>((resolve) => { pending.trades = resolve; }) : undefined });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  const panel = within(screen.getByRole("tabpanel"));
  const [matrixButton, listButton] = panel.getAllByRole("button", { name: "Рассчитать" });
  await userEvent.click(matrixButton!);
  await userEvent.click(listButton!);
  await waitFor(() => { expect(pending.matrix).toBeDefined(); expect(pending.trades).toBeDefined(); });

  fireEvent.change(panel.getByLabelText("С даты"), { target: { value: "2026-01-04T11:31" } });
  fireEvent.blur(panel.getByLabelText("С даты"));
  expect(calls.filter((call) => call.path === "/v1/analytics/matrix-grid")).toHaveLength(1);
  expect(calls.filter((call) => call.path === "/v1/analytics/trades")).toHaveLength(1);
  await userEvent.click(matrixButton!);
  expect(await panel.findByText("0.42")).toBeVisible();
  await userEvent.click(listButton!);
  expect(await panel.findByText("Матожидание: +1.00R по 1 закрытым")).toBeVisible();
  expect(panel.getByText(current.setup.symbol)).toBeVisible();
  expect(panel.queryByRole("button", { name: "Остановить" })).not.toBeInTheDocument();

  await act(async () => {
    pending.matrix!(new Response(JSON.stringify({ items: [matrixCell(4, 4, 9.99)], ...selectionMetrics })));
    pending.trades!(new Response(JSON.stringify({ items: [stale], cachedCount: 1 })));
    await Promise.resolve();
  });
  expect(panel.getByText("0.42")).toBeVisible();
  expect(panel.getByText(current.setup.symbol)).toBeVisible();
  expect(panel.getByText("Матожидание: +1.00R по 1 закрытым")).toBeVisible();
  expect(panel.queryByText("9.99")).not.toBeInTheDocument();
  expect(panel.queryByText("Матожидание: -1.00R по 1 закрытым")).not.toBeInTheDocument();
  for (const path of ["/v1/analytics/matrix-grid", "/v1/analytics/setups", "/v1/analytics/trades"]) {
    const requests = calls.filter((call) => call.path === path);
    expect(requests).toHaveLength(2);
    expect(requests.map((call) => {
      const query = new URLSearchParams(call.search);
      return [query.get("fromUnix"), query.get("toUnix")];
    })).toEqual([[String(fromUnix), String(toUnix)], [String(fromUnix + 60), String(toUnix)]]);
  }
  expect(calls.some((call) => call.path.startsWith("/v1/analytics/trades/"))).toBe(false);
});

it("blocks the edited matrix auto-run for a Berlin DST-gap boundary and uses only the completed exact pair without a list auto-run", async () => {
  applyInterfaceSettings({ timezone: "Europe/Berlin" });
  const fromUnix = Date.parse("2026-03-29T00:30:00Z") / 1000;
  const toUnix = Date.parse("2026-03-29T01:30:00Z") / 1000;
  const calls = stubFetch({ shortBulk: [matrixCell(4, 4, 0.42)], savedSettings: { autoCalc: true, trendMode: "mean",
    sections: { "short-matrix": { timeRange: { mode: "custom", fromUnix, toUnix } } } } });
  const matrixCalls = () => calls.filter((call) => call.path === "/v1/analytics/matrix-grid"
    && new URLSearchParams(call.search).get("direction") === "short");
  const listCalls = () => calls.filter((call) => call.path === "/v1/analytics/setups" || call.path.startsWith("/v1/analytics/trades"));
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  const panel = within(screen.getByRole("tabpanel"));
  expect(screen.getByRole("checkbox", { name: "Авторасчёт матриц" })).toBeChecked();
  expect(await panel.findByText("0.42")).toBeVisible();
  expect(matrixCalls()).toHaveLength(1);
  expect(panel.getByLabelText("С даты")).toHaveValue("2026-03-29T01:30");
  expect(panel.getByLabelText("По дату")).toHaveValue("2026-03-29T03:30");

  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
  try {
    fireEvent.change(panel.getByLabelText("С даты"), { target: { value: "2026-03-29T02:30" } });
    fireEvent.blur(panel.getByLabelText("С даты"));
    await act(async () => { await vi.advanceTimersByTimeAsync(1000); });
    // The shared debounce also runs LONG; only the edited SHORT scope is invalid.
    expect(matrixCalls()).toHaveLength(1);
    expect(listCalls()).toHaveLength(0);
    expect(panel.queryByText("0.42")).not.toBeInTheDocument();
    expect(panel.getByRole("alert")).toHaveTextContent("Укажите корректные даты: конец диапазона должен быть позже начала.");

    fireEvent.change(panel.getByLabelText("С даты"), { target: { value: "2026-03-29T03:31" } });
    fireEvent.blur(panel.getByLabelText("С даты"));
    fireEvent.change(panel.getByLabelText("По дату"), { target: { value: "2026-03-29T03:47" } });
    fireEvent.blur(panel.getByLabelText("По дату"));
    expect(matrixCalls()).toHaveLength(1);
    await act(async () => { await vi.advanceTimersByTimeAsync(1000); });
    expect(matrixCalls()).toHaveLength(2);
    const query = new URLSearchParams(matrixCalls()[1]!.search);
    expect(query.get("fromUnix")).toBe(String(Date.parse("2026-03-29T01:31:00Z") / 1000));
    expect(query.get("toUnix")).toBe(String(Date.parse("2026-03-29T01:47:00Z") / 1000));
    for (const field of ["month", "fromDay", "toDay"]) expect(query.has(field)).toBe(false);
    expect(panel.getByText("0.42")).toBeVisible();
    expect(panel.queryByRole("alert")).not.toBeInTheDocument();
    expect(listCalls()).toHaveLength(0);
    await act(async () => { await vi.advanceTimersByTimeAsync(1000); });
    expect(matrixCalls()).toHaveLength(2);
    expect(listCalls()).toHaveLength(0);
  } finally { vi.useRealTimers(); }
});

const fullShortGrid = shortLevels.flatMap((tp) => shortLevels.map((sl) => matrixCell(tp, sl, tp === 6 && sl === 6 ? 0.17 : 0.05)));
const fullLongGrid = longTpLevels.flatMap((tp) => longSlLevels.map((sl) => matrixCell(tp, sl, -0.3)));

it("evaluates missing cell trades with bounded parallel requests and stops the admitted batch on cancellation", async () => {
  const rows = Array.from({ length: 9 }, (_, index): TradeRow => ({ setup: { ...setupA, symbol: `TEST${index}USDT`, closeTimeUnix: setupA.closeTimeUnix + index * 60 }, direction: "short", tpPct: 4, slPct: 4, status: "tp", entryPrice: 100, tpPrice: 96, slPrice: null, closeTimeUnix: setupA.closeTimeUnix + index * 60 + 600, riskReturn: 1 }));
  const pending: { signal?: AbortSignal | undefined; resolve: (response: Response) => void }[] = [];
  const calls = stubFetch({ tradeRows: rows, interceptTradeRows: () => Promise.resolve(new Response(JSON.stringify({ items: [], cachedCount: 0 }))),
    interceptTradeRow: (_setup, signal) => new Promise<Response>((resolve) => { pending.push({ signal, resolve }); }) });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await waitFor(() => expect(pending).toHaveLength(4));
  expect(calls.filter((call) => call.path.startsWith("/v1/analytics/trades/"))).toHaveLength(4);
  await userEvent.click(screen.getByRole("button", { name: "Остановить" }));
  expect(pending.every((item) => item.signal?.aborted)).toBe(true);
  await act(async () => { pending.forEach((item, index) => item.resolve(new Response(JSON.stringify(rows[index])))); await Promise.resolve(); });
  expect(calls.filter((call) => call.path.startsWith("/v1/analytics/trades/"))).toHaveLength(4);
  expect(screen.queryByText("TEST0USDT")).not.toBeInTheDocument();
});

it("keeps the accepted trend and observed snapshot while a same-scope iteration fits its replacement", async () => {
  stubFetch(utcChartOptions([utcTrade("2026-01-01T12:00:00Z", -1, 0), utcTrade("2026-01-02T12:00:00Z", 1, 1)]));
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  const curve = await screen.findByTestId("trend-curve");
  const chart = screen.getByRole("img", { name: "Дневное матожидание в рисках" });
  const before = chart.innerHTML;
  const original = globalThis.fetch;
  let release: (() => void) | undefined;
  vi.stubGlobal("fetch", vi.fn((url: string, init?: RequestInit) => {
    const reply = original(url, init);
    return url === "/v1/analytics/trend" ? new Promise<Response>((resolve) => { void reply.then((response) => { release = () => resolve(response); }); }) : reply;
  }));
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await waitFor(() => expect(release).toBeDefined());
  expect(screen.getByTestId("trend-curve")).toBe(curve);
  expect(chart.innerHTML).toBe(before);
  await act(async () => { release!(); await Promise.resolve(); });
  expect(screen.getByTestId("trend-curve")).toHaveAttribute("points", curve.getAttribute("points"));
});

it("uses one matrix-owned selection per direction, ignores conflicting legacy list filters and preserves cell-only controls", async () => {
  const calls = stubFetch({ tradeRows: [], savedSettings: { trendMode: "mean", clusterize: false, sections: {
    "short-matrix": { timeRange: { mode: "month", month: "2024-01" }, timeShiftInput: "7", considerPriceChange: true, priceChangeInput: "0" },
    "long-matrix": { timeRange: { mode: "custom", fromDay: "2024-01-02", toDay: "2024-01-03" }, timeShiftInput: "9", considerPriceChange: true, priceChangeInput: "-1.25" },
    "short-trades": { timeRange: { mode: "month", month: "1999-01" }, timeShiftInput: "999", considerPriceChange: true, priceChangeInput: "999", tpInput: "6", slInput: "2", riskPctInput: "3" },
    "long-trades": { timeRange: { mode: "month", month: "1998-01" }, timeShiftInput: "998", considerPriceChange: false, tpInput: "8", slInput: "1", riskPctInput: "4" },
  } } });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  expect(screen.getAllByLabelText("Период")).toHaveLength(2);
  expect(screen.getAllByLabelText("Time-shift, мин")).toHaveLength(2);
  expect(screen.getAllByLabelText("Риск на сделку, %")[0]).toHaveValue("3");
  expect(calls.some((call) => call.method === "POST" || call.path.includes("/trades"))).toBe(false);
  for (const direction of ["short", "long"] as const) {
    if (direction === "long") await userEvent.click(screen.getByRole("tab", { name: "LONG" }));
    await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
    await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
    const scoped = calls.filter((call) => ["/v1/analytics/matrix-grid", "/v1/analytics/setups", "/v1/analytics/trades"].includes(call.path)
      && new URLSearchParams(call.search).get("direction") === direction);
    expect(scoped).toHaveLength(3);
    for (const call of scoped) {
      const query = new URLSearchParams(call.search);
      expect(query.get("timeShiftMinutes")).toBe(direction === "short" ? "7" : "9");
      expect(query.get("priceChangePct")).toBe(direction === "short" ? "0" : "-1.25");
      expect(query.get("fromUnix")).toBe(String(Date.parse(direction === "short" ? "2024-01-01T00:00:00Z" : "2024-01-02T00:00:00Z") / 1000));
      expect(query.get("toUnix")).toBe(String(Date.parse(direction === "short" ? "2024-02-01T00:00:00Z" : "2024-01-04T00:00:00Z") / 1000));
      for (const key of ["month", "fromDay", "toDay"]) expect(query.has(key)).toBe(false);
      expect(query.has("setupMaturity")).toBe(false);
      if (call.path !== "/v1/analytics/matrix-grid") {
        expect(query.get("tpPct")).toBe(direction === "short" ? "6" : "8");
        expect(query.get("slPct")).toBe(direction === "short" ? "2" : "1");
      }
    }
  }
});

it.each(["short", "long"] as const)("uses the old all/shift0/gate-off matrix defaults for %s, never missing-matrix list fallback", async (direction) => {
  const calls = stubFetch({ tradeRows: [], savedSettings: { trendMode: "mean", sections: {
    [`${direction}-trades`]: { timeRange: { mode: "month", month: "1999-01" }, timeShiftInput: "9", considerPriceChange: true, priceChangeInput: "-5", tpInput: "6", slInput: "2" },
  } } });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  if (direction === "long") await userEvent.click(screen.getByRole("tab", { name: "LONG" }));
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  const query = new URLSearchParams(calls.find((call) => call.path === "/v1/analytics/trades")!.search);
  for (const key of ["month", "fromDay", "toDay", "timeShiftMinutes", "priceChangePct"]) expect(query.has(key)).toBe(false);
  expect(query.get("tpPct")).toBe("6"); expect(query.get("slPct")).toBe("2");
  const count = calls.filter((call) => call.path.includes("/trades") || call.method === "POST").length;
  const panel = within(screen.getByRole("tabpanel"));
  fireEvent.change(panel.getByLabelText("Time-shift, мин"), { target: { value: "8" } });
  fireEvent.click(panel.getByRole("checkbox", { name: "Учитывать смещение цены" }));
  expect(panel.getByLabelText("Смещение цены, %")).toHaveValue("0"); // On-zero is not off.
  expect(calls.filter((call) => call.path.includes("/trades") || call.method === "POST")).toHaveLength(count);
  await userEvent.click(panel.getAllByRole("button", { name: "Рассчитать" })[1]!);
  const next = new URLSearchParams(calls.filter((call) => call.path === "/v1/analytics/trades").at(-1)!.search);
  expect(next.get("priceChangePct")).toBe("0"); expect(next.get("timeShiftMinutes")).toBe("8");
});

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
  expect(screen.getAllByLabelText("С даты")[0]).toHaveValue("2023-11-14T00:00");
  expect(screen.getAllByLabelText("По дату")[0]).toHaveValue("2023-11-16T00:00");
  expect(screen.getByRole("heading", { name: "Среднее матожидание сетапов" })).toBeInTheDocument();
  expect(screen.getByRole("heading", { name: "Расчёт ячейки" })).toBeInTheDocument();
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
  expect(screen.queryByText("0.17")).not.toBeInTheDocument(); // Scope edits invalidate, not restore, the old result.
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
  await waitFor(() => expect(screen.getAllByLabelText<HTMLSelectElement>("Период").every((select) => [...select.options].some((option) => option.value === "2024-02"))).toBe(true));
  fireEvent.change(screen.getAllByLabelText("Период")[0]!, { target: { value: "2024-01" } });
  expect(screen.getAllByLabelText("С даты")[0]).toHaveValue("2024-01-01T00:00");
  expect(screen.getAllByLabelText("По дату")[0]).toHaveValue("2024-02-01T00:00");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  await waitFor(() => expect(screen.getByText("Ячеек: 196 / 196")).toBeInTheDocument());
  const monthQuery = new URLSearchParams(calls.find((call) => call.path === "/v1/analytics/matrix-grid")!.search);
  expect(monthQuery.get("fromUnix")).toBe(String(Date.parse("2024-01-01T00:00:00Z") / 1000));
  expect(monthQuery.get("toUnix")).toBe(String(Date.parse("2024-02-01T00:00:00Z") / 1000));
  expect(monthQuery.has("month")).toBe(false);

  fireEvent.change(screen.getAllByLabelText("С даты")[0]!, { target: { value: "2024-01-10T12:35" } });
  expect(screen.getAllByLabelText("Период")[0]).toHaveValue("custom");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  await waitFor(() => expect(screen.getByText("Ячеек: 196 / 196")).toBeInTheDocument());
  const custom = new URLSearchParams(calls.filter((call) => call.path === "/v1/analytics/matrix-grid").at(-1)!.search);
  expect(custom.get("fromUnix")).toBe(String(Date.parse("2024-01-10T12:35:00Z") / 1000));
  expect(custom.get("toUnix")).toBe(monthQuery.get("toUnix"));
  expect(custom.has("month") || custom.has("fromDay")).toBe(false);
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
  expect(screen.getAllByLabelText("С даты")[0]).toHaveValue("2024-01-01T00:00");
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
  await waitFor(() => expect(screen.getAllByLabelText<HTMLSelectElement>("Период").every((select) => [...select.options].some((option) => option.value === "2024-02"))).toBe(true), { timeout: 5000 });
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
  const heading = screen.getByRole("heading", { name: /^Дневное матожидание в рисках/ });
  expect(heading).toHaveTextContent("Среднее МО: +0.00R · Винрейт: 50.0%");
  expect(within(heading).getByLabelText("Винрейт за период")).toHaveTextContent("50.0%");
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

it("rejects reversed exact ranges without starting matrix or list calculation", async () => {
  const calls = stubFetch();
  render(<AnalyticsPage />);
  await screen.findByText("#12 · Утренний прогон");
  fireEvent.change(screen.getAllByLabelText("С даты")[0]!, { target: { value: "2124-01-01T00:00" } });
  expect(screen.getAllByLabelText("Период")[0]).toHaveValue("custom");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  expect(screen.getAllByText("Укажите корректные даты: конец диапазона должен быть позже начала.")).toHaveLength(2);
  expect(calls.some((call) => call.path.includes("/matrix-grid") || call.path.includes("/setups") || call.path.includes("/trades"))).toBe(false);
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

function utcTrade(at: string, riskReturn: number | null, index: number): TradeRow {
  const closeTimeUnix = Date.parse(at) / 1000;
  return { setup: { ...setupA, symbol: `UTC${index}USDT`, openTimeUnix: closeTimeUnix - 60, closeTimeUnix },
    direction: "short", tpPct: 4, slPct: 4, status: riskReturn === null ? "unclosed" : riskReturn < 0 ? "sl" : "tp",
    entryPrice: 100, tpPrice: 96, slPrice: 104, closeTimeUnix: closeTimeUnix + 86400, riskReturn };
}
function utcChartOptions(rows: TradeRow[]): StubOptions {
  return { sets: [{ ...setFixture, fromUnix: Date.parse("2026-01-01T00:00:00Z") / 1000, toUnix: Date.parse("2026-01-12T00:00:00Z") / 1000 }], tradeRows: rows };
}

it.each([
  { start: "2026-03-08T00:00:00Z", hours: 23 },
  { start: "2026-11-01T00:00:00Z", hours: 25 },
])("projects the trend chart onto $hours-hour New York display days without changing UTC solver inputs", async ({ start, hours }) => {
  applyInterfaceSettings({ timezone: "America/New_York" });
  const from = Date.parse(start) / 1000;
  const rows = [0, 1, 2].map((offset) => utcTrade(new Date((from + offset * 86400 + 3600) * 1000).toISOString(), offset % 2 === 0 ? 1 : -1, offset));
  const calls = stubFetch({ tradeRows: rows, sets: [{ ...setFixture, fromUnix: from, toUnix: from + 3 * 86400 }] });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await screen.findByTestId("trend-curve");
  const chart = screen.getByRole("img");
  const lines = [...within(chart).getByTestId("daily-calendar-grid").querySelectorAll("line")];
  const boundaries = lines.map((line) => Number(line.getAttribute("data-day-start-unix")));
  expect((boundaries[2]! - boundaries[1]!) / 3600).toBe(hours);
  [...chart.querySelectorAll("circle")].forEach((circle, index) => expect(circle.getAttribute("cx")).toBe(lines[index]!.getAttribute("x1")));
  const request = JSON.parse(calls.find((call) => call.path === "/v1/analytics/trend")!.body!) as { points: TrendInput[] };
  expect(request.points.map((point) => point.day)).toEqual([0, 1, 2].map((offset) => from / 86400 + offset));
  const count = calls.length;
  act(() => { applyInterfaceSettings({ timezone: "Asia/Kathmandu" }); });
  expect(calls).toHaveLength(count);
  expect(boundaries).not.toEqual([...within(chart).getByTestId("daily-calendar-grid").querySelectorAll("line")].map((line) => Number(line.getAttribute("data-day-start-unix"))));
});

it.each([
  { timezone: "America/New_York", dates: ["31.12.2025", "03.01.2026", "09.01.2026"] },
  { timezone: "Pacific/Auckland", dates: ["01.01.2026", "04.01.2026", "10.01.2026"] },
])("maps unchanged UTC buckets to the configured calendar, not the browser zone ($timezone)", async ({ timezone, dates }) => {
  const DateTimeFormat = Intl.DateTimeFormat;
  vi.spyOn(Intl, "DateTimeFormat").mockImplementation(function (locales, options) { return new DateTimeFormat(locales, { ...options, timeZone: options?.timeZone ?? "Asia/Tokyo" }); });
  expect(Intl.DateTimeFormat().resolvedOptions().timeZone).toBe("Asia/Tokyo");
  applyInterfaceSettings({ timezone });
  const rows = [utcTrade("2026-01-01T00:00:00Z", 2, 0), utcTrade("2026-01-01T12:00:00Z", 2, 1), utcTrade("2026-01-01T23:59:00Z", -1, 2),
    utcTrade("2026-01-03T12:00:00Z", null, 3), utcTrade("2026-01-04T12:00:00Z", -1, 4), utcTrade("2026-01-10T12:00:00Z", 2, 5)]
    .map((item) => ({ ...item, tpPct: 2, slPct: 1, tpPrice: 98, slPrice: 101 }));
  const calls = stubFetch(utcChartOptions(rows));
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  expect(calls.some((call) => call.path.startsWith("/v1/analytics/trades") || call.path === "/v1/analytics/setups")).toBe(false);
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await screen.findByText("Сделок: 6 / 6");
  const chart = screen.getByRole("img", { name: "Дневное матожидание в рисках" });
  const circles = [...chart.querySelectorAll("circle")];
  expect(circles.map((circle) => Number(circle.getAttribute("cx")))).toEqual([64, 288, 736]); // The projected calendar preserves the 0, 3, 9 day gaps.
  expect(circles.map((circle) => circle.querySelector("title")?.textContent)).toEqual([
    `${dates[0]} (${timezone}): +1.00R, закрыто 3, TP 2, SL 1, из кеша 3/3`,
    `${dates[1]} (${timezone}): -1.00R, закрыто 1, TP 0, SL 1, из кеша 1/1`,
    `${dates[2]} (${timezone}): +2.00R, закрыто 1, TP 1, SL 0, из кеша 1/1`,
  ]);
  expect([...chart.querySelectorAll('text[y="254"]')].map((label) => label.firstChild?.textContent)).toEqual(timezone === "America/New_York"
    ? ["31.12", "02.01", "03.01", "09.01"]
    : ["01.01", "03.01", "04.01", "10.01"]);
  expect(chart.querySelector('polyline:not([data-testid])')?.getAttribute("points")?.split(" ")).toHaveLength(2); // Null day breaks the observed series, not the fitted curve.
  expect(await screen.findByLabelText("Начало тренда: +0.59R")).toHaveTextContent("+0.59R");
  expect(screen.getByLabelText("Конец тренда: +1.38R")).toHaveTextContent("+1.38R");
  expect(chart.querySelector('[data-testid="trend-curve"] title')).toHaveTextContent("Взвешенная прямая: изменение +0.79R");
  expect(chart).toHaveTextContent(`Дата сетапа (${timezone})`);
  expect(chart.querySelector("desc")).toHaveTextContent(`календарным суткам ${timezone} без пересчёта исходных значений`);
  expect([...chart.querySelectorAll("title")].some((title) => title.textContent?.includes(" UTC:"))).toBe(false);
  expect(chart).not.toHaveTextContent("Дата сигнала");
  expect(screen.getByRole("heading", { name: /^Дневное матожидание в рисках/ })).toHaveTextContent("Среднее МО: +0.80R");
  expect(screen.getByLabelText("Среднее МО за период")).toHaveClass("text-success");
  const zero = screen.getByLabelText("Нулевой уровень");
  expect(zero).toHaveTextContent(/^0$/);
  expect(Number(zero.getAttribute("x"))).toBeLessThan(64);
  expect(Number(chart.querySelector('line[class="stroke-muted-foreground/50"]')?.getAttribute("y1"))).toBeCloseTo(Number(zero.getAttribute("y")) - 4, 8);
  expect(circles[0]).toHaveClass("fill-success"); expect(circles[1]).toHaveClass("fill-danger");
  expect(screen.getByText("Win rate: 60.0%")).toBeInTheDocument();
  expect(screen.getByRole("heading", { name: /^Дневное матожидание в рисках/ })).toHaveTextContent("Винрейт: 60.0%");
  expect(screen.getByText("Матожидание: +0.80R по 5 закрытым")).toBeInTheDocument(); // Trade-weighted, not 2/3 daily mean.
  expect(calls.filter((call) => call.path === "/v1/analytics/trades")).toHaveLength(1);
  expect(calls.some((call) => call.path.startsWith("/v1/analytics/trades/"))).toBe(false);
  expect(calls.filter((call) => call.method !== "GET").map((call) => call.path)).toEqual(["/v1/analytics/trend"]);
  const trendRequest = JSON.parse(calls.find((call) => call.path === "/v1/analytics/trend")!.body!) as { points: TrendInput[] };
  expect(trendRequest.points).toEqual([
    { day: Date.parse("2026-01-01T00:00:00Z") / 86400000, value: 1, weight: 3 },
    { day: Date.parse("2026-01-03T00:00:00Z") / 86400000, value: null, weight: 1 },
    { day: Date.parse("2026-01-04T00:00:00Z") / 86400000, value: -1, weight: 1 },
    { day: Date.parse("2026-01-10T00:00:00Z") / 86400000, value: 2, weight: 1 },
  ]);
  const curveBefore = screen.getByTestId("trend-curve").getAttribute("points");
  act(() => { applyInterfaceSettings({ timezone: "UTC", dateOrder: "ymd", dateSeparator: "-" }); });
  expect(screen.getByRole("img", { name: "Дневное матожидание в рисках" })).toBe(chart);
  expect(chart).toHaveTextContent("Дата сетапа (UTC)");
  expect(circles[0]?.querySelector("title")).toHaveTextContent("2026-01-01 (UTC): +1.00R, закрыто 3");
  expect(screen.getByTestId("trend-curve")).toHaveAttribute("points", curveBefore);
  expect(calls.filter((call) => call.path === "/v1/analytics/trend")).toHaveLength(1);
  expect(calls.filter((call) => call.path === "/v1/analytics/trades")).toHaveLength(1);
});

it.each([
  { name: "dense", offsets: Array.from({ length: 16 }, (_, index) => index) },
  { name: "sparse", offsets: [0, 5, 15] },
])("keeps a full daily grid behind the $name legacy series without filling missing observations", async ({ offsets }) => {
  applyInterfaceSettings({ timezone: "America/New_York" });
  const first = Date.parse("2026-01-01T00:00:00Z") / 1000;
  const rows = offsets.map((offset, index) => utcTrade(new Date((first + offset * 86400) * 1000).toISOString(), offset === 5 ? null : offset % 2 === 0 ? 1 : -1, index));
  const calls = stubFetch({ ...utcChartOptions(rows),
    sets: [{ ...setFixture, fromUnix: first, toUnix: first + 16 * 86400 }], trend: null });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await screen.findByText(`Сделок: ${rows.length} / ${rows.length}`);
  const chart = screen.getByRole("img", { name: "Дневное матожидание в рисках" });
  const grid = within(chart).getByTestId("daily-calendar-grid");
  const lines = [...grid.querySelectorAll("line")];
  expect(lines).toHaveLength(16);
  expect(lines.map((line) => Number(line.getAttribute("data-day-start-unix")))).toEqual(Array.from({ length: 16 }, (_, offset) => zonedDayStart(first + offset * 86400)));
  expect(lines.map((line) => Number(line.getAttribute("x1")))).toEqual(Array.from({ length: 16 }, (_, offset) => 64 + offset / 15 * 672));
  for (const line of lines) {
    expect(line.getAttribute("x2")).toBe(line.getAttribute("x1"));
    expect(line).toHaveAttribute("y1", "24"); expect(line).toHaveAttribute("y2", "224");
  }
  const circles = [...chart.querySelectorAll("circle")];
  const closed = rows.filter((row) => row.riskReturn !== null);
  expect(circles).toHaveLength(closed.length);
  closed.forEach((row, index) => {
    const line = grid.querySelector(`line[data-day-start-unix="${zonedDayStart(row.setup.closeTimeUnix)}"]`)!;
    expect(circles[index]!.getAttribute("cx")).toBe(line.getAttribute("x1"));
  });
  expect(circles.some((circle) => circle.getAttribute("cx") === lines[5]!.getAttribute("x1"))).toBe(false);
  expect(chart.querySelectorAll('text[y="254"]')).toHaveLength(offsets.length > 8 ? 9 : offsets.length);
  if (offsets.length > 8) expect(grid.compareDocumentPosition(chart.querySelector("polyline")!) & Node.DOCUMENT_POSITION_FOLLOWING).not.toBe(0);
  await waitFor(() => expect(calls.filter((call) => call.path === "/v1/analytics/trend")).toHaveLength(1));
  const request = JSON.parse(calls.find((call) => call.path === "/v1/analytics/trend")!.body!) as { points: TrendInput[] };
  expect(request.points).toEqual(rows.map((row) => ({ day: row.setup.closeTimeUnix / 86400, value: row.riskReturn, weight: 1 })));
  const geometry = [...lines.map((line) => line.getAttribute("x1")), ...circles.map((circle) => circle.getAttribute("cx"))];
  act(() => { applyInterfaceSettings({ timezone: "Pacific/Auckland" }); });
  expect([...grid.querySelectorAll("line"), ...chart.querySelectorAll("circle")].map((element) => element.getAttribute(element.tagName === "line" ? "x1" : "cx"))).toEqual(geometry);
  expect(calls.filter((call) => call.path === "/v1/analytics/trend")).toHaveLength(1);
  expect(calls.filter((call) => call.path === "/v1/analytics/trades")).toHaveLength(1);
});

it.each([
  { risk: 1, label: "+1.00R", color: "text-success" },
  { risk: -1, label: "-1.00R", color: "text-danger" },
  { risk: 0, label: "+0.00R", color: "text-muted-foreground" },
  { risk: -0.001, label: "+0.00R", color: "text-danger" },
])("labels zero and colors the period mean before rounding ($risk)", async ({ risk, label, color }) => {
  stubFetch(utcChartOptions([utcTrade("2026-01-04T12:00:00Z", risk, 0)]));
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await screen.findByText("Сделок: 1 / 1");
  const mean = screen.getByLabelText("Среднее МО за период");
  expect(mean).toHaveTextContent(label); expect(mean).toHaveClass(color);
  expect(screen.getByLabelText("Нулевой уровень")).toHaveTextContent(/^0$/);
});

it.each([
  { risks: [0.25, 0.75], upper: "+1.00R", lower: "0" },
  { risks: [-2, -0.25], upper: "+1.00R", lower: "-2.00R" },
  { risks: [-1, 2], upper: "+2.00R", lower: "-1.00R" },
  { risks: [0, 0], upper: "+1.00R", lower: "0" },
])("uses exact displayed extrema with a minimum [0, 1] vertical range ($risks)", async ({ risks, upper, lower }) => {
  stubFetch({ ...utcChartOptions(risks.map((risk, index) => utcTrade(`2026-01-0${index + 1}T12:00:00Z`, risk, index))), trend: null });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await screen.findByText("Сделок: 2 / 2");
  const chart = screen.getByRole("img", { name: "Дневное матожидание в рисках" });
  const topLabel = within(chart).getByText(upper, { selector: "text" });
  const bottomLabel = within(chart).getByText(lower, { selector: "text" });
  expect(topLabel).toHaveAttribute("y", "28");
  expect(bottomLabel).toHaveAttribute("y", "228");
  expect(topLabel).toHaveAttribute("text-anchor", "end");
  expect(bottomLabel).toHaveAttribute("text-anchor", "end");
  expect(Number(screen.getByLabelText("Нулевой уровень").getAttribute("x"))).toBeLessThan(64);
});

it("updates the chart mean and TP/(TP+SL) win rate for only the visible period", async () => {
  stubFetch(utcChartOptions([
    utcTrade("2026-01-01T12:00:00Z", 2, 0), utcTrade("2026-01-01T13:00:00Z", 2, 1),
    utcTrade("2026-01-04T12:00:00Z", -1, 2), utcTrade("2026-01-04T13:00:00Z", null, 3),
  ]));
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await screen.findByText("Сделок: 4 / 4");
  const heading = screen.getByRole("heading", { name: /^Дневное матожидание в рисках/ });
  expect(heading).toHaveTextContent("Среднее МО: +1.00R · Винрейт: 66.7%");
  expect(within(heading).getByLabelText("Среднее МО за период")).toHaveClass("text-success");
  fireEvent.change(screen.getAllByLabelText("С даты")[0]!, { target: { value: "2026-01-04T00:00" } });
  expect(screen.queryByRole("img", { name: "Дневное матожидание в рисках" })).not.toBeInTheDocument();
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  const updated = await screen.findByRole("heading", { name: /^Дневное матожидание в рисках/ });
  expect(updated).toHaveTextContent("Среднее МО: -1.00R · Винрейт: 0.0%");
  expect(within(updated).getByLabelText("Среднее МО за период")).toHaveClass("text-danger");
});

it("renders a single UTC-day point centered without a fabricated trend or endpoint labels", async () => {
  stubFetch(utcChartOptions([utcTrade("2026-01-04T00:00:00Z", 1, 0)]));
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await screen.findByText("Сделок: 1 / 1");
  const chart = screen.getByRole("img", { name: "Дневное матожидание в рисках" });
  expect(chart.querySelectorAll("circle")).toHaveLength(1); expect(chart.querySelector("circle")).toHaveAttribute("cx", "400");
  const grid = within(chart).getByTestId("daily-calendar-grid");
  expect(grid.querySelectorAll("line")).toHaveLength(1);
  expect(grid.querySelector("line")).toHaveAttribute("x1", "400");
  expect(chart.querySelector('line[stroke-dasharray]')).toBeNull();
  expect(screen.queryByLabelText(/^Начало тренда:/)).not.toBeInTheDocument(); expect(screen.queryByLabelText(/^Конец тренда:/)).not.toBeInTheDocument();
});

it("shows an explicit no-closed-days state instead of manufacturing zero points", async () => {
  stubFetch(utcChartOptions([utcTrade("2026-01-04T00:00:00Z", null, 0)]));
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await screen.findByText("Пока нет дней с закрытыми TP/SL сделками.");
  expect(screen.queryByRole("img", { name: "Дневное матожидание в рисках" })).not.toBeInTheDocument();
});

it("keeps both fitted endpoint values in the SVG viewport even when OLS extends outside observed extrema", async () => {
  stubFetch(utcChartOptions([utcTrade("2026-01-01T12:00:00Z", 0, 0), utcTrade("2026-01-02T12:00:00Z", 1, 1), utcTrade("2026-01-03T12:00:00Z", 1, 2)]));
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await screen.findByText("Сделок: 3 / 3");
  const start = await screen.findByLabelText("Начало тренда: +0.17R");
  const end = screen.getByLabelText("Конец тренда: +1.17R");
  for (const label of [start, end]) {
    expect(Number(label.getAttribute("y"))).toBeGreaterThan(10); expect(Number(label.getAttribute("y"))).toBeLessThan(224);
  }
  expect(start).toHaveAttribute("text-anchor", "start"); expect(end).toHaveAttribute("text-anchor", "end");
});

it("restores trend controls, sends forecast cache dimensions and exposes matrix uncertainty", async () => {
  const calls = stubFetch({ savedSettings: { trendBendDays: "14", trendMode: "forecast" }, shortBulk: [{ ...matrixCell(4, 4, 0.42), endpointSe: 0.15 }] });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  expect(screen.getByLabelText("Перегиб на N дней")).toHaveValue("14");
  expect(screen.getByRole("button", { name: "Прогноз" })).toHaveAttribute("aria-pressed", "true");
  expect(screen.getByRole("heading", { name: "Прогноз матожидания по тренду" })).toBeInTheDocument();
  expect(calls.some((call) => call.path === "/v1/analytics/matrix-grid")).toBe(false);
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  const cell = await screen.findByText("0.42");
  expect(cell).toHaveAttribute("title", expect.stringContaining("SE 0.150 R · 95% ±0.294 R"));
  const query = new URLSearchParams(calls.find((call) => call.path === "/v1/analytics/matrix-grid")!.search);
  expect(query.get("trendMode")).toBe("forecast"); expect(query.get("trendBendDays")).toBe("14");
  // A changed forecast identity cannot relabel already-computed cells.
  fireEvent.change(screen.getByLabelText("Перегиб на N дней"), { target: { value: "7" } });
  expect(screen.queryByText("0.42")).not.toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "Среднее" }));
  expect(screen.getByRole("button", { name: "Среднее" })).toHaveAttribute("aria-pressed", "true");
  await waitFor(() => expect(calls.some((call) => call.method === "PUT")).toBe(true), { timeout: 3000 });
  const saved = JSON.parse(calls.findLast((call) => call.method === "PUT")!.body!) as Record<string, unknown>;
  expect(saved).toMatchObject({ trendMode: "mean", trendBendDays: "7" });
});

it("draws only the server P-spline, scales to its extrema without CI and preserves the trade mean", async () => {
  const day = Date.parse("2026-01-01T00:00:00Z") / 86400000;
  const curve = [
    { day, value: -0.5, se: 0.1 }, { day: day + 1, value: 1.5, se: 0.3 },
    { day: day + 2, value: -0.25, se: 0.5 }, { day: day + 3, value: 0.75, se: 0.7 },
  ];
  const calls = stubFetch({ ...utcChartOptions([0, 1, 2, 3].map((i) => utcTrade(`2026-01-0${i + 1}T12:00:00Z`, i % 2 === 0 ? -1 : 1, i))),
    savedSettings: { trendBendDays: "7" }, trend: { method: "p_spline", start: curve[0]!, end: curve[3]!, endpointSe: 0.7, curve } });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  const line = await screen.findByTestId("trend-curve");
  expect(line.querySelector("title")).toHaveTextContent("P-spline");
  const vertices = line.getAttribute("points")!.split(" ").map((p) => p.split(",").map(Number));
  expect(vertices).toHaveLength(4); expect(vertices[0]![0]).toBe(64); expect(vertices[3]![0]).toBe(736);
  expect(screen.queryByTestId("trend-ci-band")).not.toBeInTheDocument();
  const chart = screen.getByRole("img", { name: "Дневное матожидание в рисках" });
  expect(chart.querySelector("polygon")).toBeNull();
  expect(line).toHaveClass("fill-none", "stroke-warning");
  expect(vertices.every(([x, y]) => x! >= 64 && x! <= 736 && y! >= 24 && y! <= 224)).toBe(true);
  expect(within(chart).getByText("+1.50R", { selector: "text" })).toHaveAttribute("y", "28");
  expect(within(chart).getByText("-1.00R", { selector: "text" })).toHaveAttribute("y", "228");
  expect(screen.getByLabelText("Конец тренда: +0.75R")).toBeInTheDocument();
  expect(screen.getByLabelText("Среднее МО за период")).toHaveTextContent("+0.00R");
  const request = JSON.parse(calls.find((call) => call.path === "/v1/analytics/trend")!.body!) as { trendBendDays: string };
  expect(request.trendBendDays).toBe("7");
});

it("reports curve failures and aborts owned curve work on replacement and unmount", async () => {
  const pending: AbortSignal[] = [];
  let attempts = 0;
  stubFetch({ ...utcChartOptions([utcTrade("2026-01-01T12:00:00Z", -1, 0), utcTrade("2026-01-02T12:00:00Z", 1, 1)]), interceptTrend: (signal) => {
    attempts += 1;
    if (attempts === 1) return Promise.resolve(new Response("unavailable", { status: 503 }));
    pending.push(signal!); return new Promise<Response>(() => undefined);
  } });
  const view = render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  expect(await screen.findByText(/Не удалось рассчитать тренд: HTTP 503/)).toBeInTheDocument();
  expect(screen.queryByTestId("trend-curve")).not.toBeInTheDocument();
  fireEvent.change(screen.getByLabelText("Перегиб на N дней"), { target: { value: "7" } });
  await waitFor(() => expect(pending).toHaveLength(1));
  fireEvent.change(screen.getByLabelText("Перегиб на N дней"), { target: { value: "14" } });
  expect(pending[0]!.aborted).toBe(true);
  await waitFor(() => expect(pending).toHaveLength(2));
  view.unmount(); expect(pending[1]!.aborted).toBe(true);
});

const probabilityRecipe = { method: "decayed_probability", version: 1, halfLifeDays: 14, kappa: 10, priorPolicy: "gross_break_even" } as const;
const probabilityCutoff = Date.parse("2026-01-13T00:00:00Z") / 1000;
function probabilityOptions(): StubOptions {
  const rows = [utcTrade("2026-01-01T12:00:00Z", 2, 0), utcTrade("2026-01-01T13:00:00Z", 2, 1), utcTrade("2026-01-04T12:00:00Z", -1, 2),
    utcTrade("2026-01-10T12:00:00Z", 2, 3), utcTrade("2026-01-10T13:00:00Z", -1, 4)].map((row) => ({ ...row, tpPct: 2, slPct: 1 }));
  return { ...utcChartOptions(rows), savedSettings: { trendMode: "blend", trendBlendLambda: "0.7", forecast: probabilityRecipe,
    sections: { "short-trades": { tpInput: "2", slInput: "1" } } }, shortBulk: [matrixCell(4, 4, 0.52)] };
}

it("keeps one seconds cutoff across matrix and graph and renders separate backend M/F/decision and actual model lines", async () => {
  const clock = vi.spyOn(Date, "now").mockReturnValue(probabilityCutoff * 1000);
  const calls = stubFetch(probabilityOptions());
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  expect(screen.getByLabelText("Метод прогноза")).toHaveValue("decayed_probability");
  expect(screen.queryByLabelText("Перегиб на N дней")).not.toBeInTheDocument();
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  const cell = await screen.findByText("0.52");
  expect(cell).toHaveAttribute("title", expect.stringContaining("M 0.800 R · F 0.400 R · decisionR 0.520 R"));
  clock.mockReturnValue((probabilityCutoff + 3600) * 1000);
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  const curve = await screen.findByTestId("forecast-model-curve");
  expect(screen.getByLabelText("Среднее модели за период")).toHaveTextContent("0.8 R");
  expect(screen.getByLabelText("Чистый прогноз модели")).toHaveTextContent("0.4 R");
  expect(screen.getByLabelText("Метрика решения")).toHaveTextContent("0.52 R");
  const mean = screen.getByTestId("period-mean");
  expect(mean.getAttribute("y1")).toBe(mean.getAttribute("y2"));
  expect(screen.queryByTestId("forecast-flat-future")).not.toBeInTheDocument();
  const endpoint = screen.getByTestId("forecast-endpoint-label");
  expect(endpoint).toHaveTextContent("0.52 R");
  expect(endpoint).toHaveAttribute("data-canonical-at-unix", String(probabilityCutoff));
  expect(endpoint).toHaveAttribute("data-signal-day-unix", "1768003200");
  expect(screen.getByTestId("forecast-blend-curve").getAttribute("points")).not.toBe(curve.getAttribute("points"));
  expect(curve.getAttribute("points")!.split(" ")).toHaveLength(3);
  expect(screen.getByText(/Ретроспективная модельная оценка, не causal\/OOS/)).toBeInTheDocument();
  expect(screen.getByText(/κ не считается реальными сделками/)).toBeInTheDocument();
  const matrix = new URLSearchParams(calls.find((call) => call.path === "/v1/analytics/matrix-grid")!.search);
  const graph = JSON.parse(calls.find((call) => call.path === "/v1/analytics/forecast")!.body!) as ForecastCurveRequest;
  expect(JSON.parse(matrix.get("forecast")!)).toEqual(graph.forecast);
  expect(Number(matrix.get("observationCutoff"))).toBe(probabilityCutoff);
  expect(graph.observationCutoff).toBe(probabilityCutoff);
  expect(graph.q).toBe(2); expect(graph.atUnix).toBe(probabilityCutoff + 86400);
  expect(graph.daily.map((day) => [day.dayStartUnix, day.tpCount, day.slCount])).toEqual([
    [1767225600, 2, 0], [1767484800, 0, 1], [1768003200, 1, 1],
  ]);
  expect(calls.some((call) => call.path === "/v1/analytics/trend")).toBe(false);
  fireEvent.change(screen.getByLabelText("Память модели H, дней"), { target: { value: "28" } });
  expect(screen.queryByText("0.52")).not.toBeInTheDocument();
});

it("keeps fresh Analytics in mean mode while an explicit new forecast selection uses probability", async () => {
  const calls = stubFetch({ savedSettings: {}, shortBulk: fullShortGrid });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  expect(screen.getByRole("button", { name: "Среднее" })).toHaveAttribute("aria-pressed", "true");
  expect(screen.queryByLabelText("Память модели H, дней")).not.toBeInTheDocument();
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  await screen.findByText("Ячеек: 196 / 196");
  expect(new URLSearchParams(calls.find((call) => call.path === "/v1/analytics/matrix-grid")!.search).has("forecast")).toBe(false);
  fireEvent.click(screen.getByRole("button", { name: "Прогноз" }));
  expect(screen.getByLabelText("Метод прогноза")).toHaveValue("decayed_probability");
  expect(screen.getByLabelText("Память модели H, дней")).toHaveValue(14);
});

it("shows unavailable probability with its reason, never a prior-only zero or a trend fallback", async () => {
  vi.spyOn(Date, "now").mockReturnValue(probabilityCutoff * 1000);
  const calls = stubFetch({ ...probabilityOptions(), forecast: (request) => {
    const body: ForecastCurveResponse = { ...forecastFixture(request), estimate: { status: "unavailable", reason: "no_weighted_evidence" },
      meanR: null, forecastR: null, decisionR: null, curve: [], flatFuture: { fromUnix: request.observationCutoff, toUnix: request.atUnix!, forecastR: null, decisionR: null } };
    delete body.blendCurve;
    delete body.displayCurve;
    return Promise.resolve(new Response(JSON.stringify(body)));
  } });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  expect(await screen.findByText(/Прогноз недоступен: нет положительного веса наблюдений/)).toBeInTheDocument();
  expect(screen.getByLabelText("Чистый прогноз модели")).toHaveTextContent("—");
  expect(screen.queryByTestId("forecast-model-curve")).not.toBeInTheDocument();
  expect(screen.queryByTestId("forecast-flat-future")).not.toBeInTheDocument();
  expect(calls.some((call) => call.path === "/v1/analytics/trend")).toBe(false);
});

it("aborts owned probability requests when recipe changes or chart unmounts, without showing stale curves", async () => {
  vi.spyOn(Date, "now").mockReturnValue(probabilityCutoff * 1000);
  const pending: AbortSignal[] = [];
  stubFetch({ ...probabilityOptions(), forecast: (_request, signal) => {
    pending.push(signal!); return new Promise<Response>(() => undefined);
  } });
  const view = render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await waitFor(() => expect(pending).toHaveLength(1));
  fireEvent.change(screen.getByLabelText("Память модели H, дней"), { target: { value: "28" } });
  expect(pending[0]!.aborted).toBe(true);
  expect(screen.queryByTestId("forecast-model-curve")).not.toBeInTheDocument();
  await waitFor(() => expect(pending).toHaveLength(2));
  view.unmount(); expect(pending[1]!.aborted).toBe(true);
});

it("splits nullable forecast and blend history without zero points or bridges and retains F(C)", async () => {
  vi.spyOn(Date, "now").mockReturnValue(probabilityCutoff * 1000);
  stubFetch({ ...probabilityOptions(), forecast: (request) => {
    const response = forecastFixture(request);
    response.curve[1]!.forecastR = null;
    response.blendCurve = response.curve.map((point) => ({ atUnix: point.atUnix, decisionR: point.forecastR === null ? null : 0.52 }));
    return Promise.resolve(new Response(JSON.stringify(withDisplayCurve(request, response))));
  } });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await screen.findByTestId("forecast-endpoint-label");
  expect(screen.queryByTestId("forecast-model-curve")).not.toBeInTheDocument();
  expect(screen.getAllByTestId("forecast-model-point")).toHaveLength(2);
  expect(screen.queryByTestId("forecast-blend-curve")).not.toBeInTheDocument();
  expect(screen.getAllByTestId("forecast-blend-point")).toHaveLength(2);
  expect(screen.getByLabelText("Чистый прогноз модели")).toHaveTextContent("0.4 R");
  expect(screen.getByLabelText("Метрика решения")).toHaveTextContent("0.52 R");
  expect(screen.queryByText(/Модель недоступна:/)).not.toBeInTheDocument();
});

it("renders all-null historical prefixes as no closed evidence, never a zero forecast or transport error", async () => {
  vi.spyOn(Date, "now").mockReturnValue(probabilityCutoff * 1000);
  stubFetch({ ...probabilityOptions(), forecast: (request) => {
    const response = forecastFixture(request);
    response.estimate = { status: "unavailable", reason: "no_closed_outcomes" };
    response.meanR = null; response.forecastR = null; response.decisionR = null;
    response.curve = response.curve.map((point) => ({ ...point, forecastR: null }));
    response.blendCurve = response.blendCurve!.map((point) => ({ ...point, decisionR: null }));
    response.flatFuture.forecastR = null; response.flatFuture.decisionR = null;
    return Promise.resolve(new Response(JSON.stringify(withDisplayCurve(request, response))));
  } });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  expect(await screen.findByText(/Прогноз недоступен: нет наблюдавшихся закрытых TP\/SL/)).toBeInTheDocument();
  for (const id of ["forecast-model-curve", "forecast-model-point", "forecast-blend-curve", "forecast-blend-point", "forecast-flat-future"]) expect(screen.queryByTestId(id)).not.toBeInTheDocument();
  expect(screen.getByLabelText("Чистый прогноз модели")).toHaveTextContent("—");
  expect(screen.queryByText(/Модель недоступна:/)).not.toBeInTheDocument();
});

it("blocks unsupported saved recipe versions rather than replacing them with the new default", async () => {
  const calls = stubFetch({ savedSettings: { trendMode: "forecast", forecast: { ...probabilityRecipe, version: 2 } } });
  render(<AnalyticsPage />);
  expect(await screen.findByRole("alert")).toHaveTextContent(/Сохранённый рецепт не заменён default/);
  expect(screen.getByRole("button", { name: "Прогноз" })).toBeDisabled();
  expect(screen.queryAllByRole("button", { name: "Рассчитать" })).toHaveLength(0);
  expect(calls.every((call) => call.method === "GET")).toBe(true);
});

it.each(["legacy", "trend", "trend_sum_r", "decayed_probability"] as const)("fences late matrix/list results after a shared scope edit for %s, with no automatic replacement run", async (method) => {
  const current = utcTrade("2026-01-04T12:00:00Z", -1, 0);
  const stale = { ...current, setup: { ...current.setup, symbol: "STALEUSDT" }, riskReturn: 9.99 };
  const gridReply: { resolve?: (value: Response) => void } = {};
  const tradesReply: { resolve?: (value: Response) => void } = {};
  let tradeAttempts = 0;
  const forecast = method === "legacy" ? undefined : method === "decayed_probability" ? probabilityRecipe
    : { method, version: 1, bendDays: "linear" };
  const calls = stubFetch({ ...utcChartOptions([current]), savedSettings: { trendMode: "forecast", ...(forecast ? { forecast } : {}) },
    interceptMatrixGrid: () => new Promise<Response>((resolve) => { gridReply.resolve = resolve; }),
    interceptTradeRows: () => ++tradeAttempts === 1 ? new Promise<Response>((resolve) => { tradesReply.resolve = resolve; }) : undefined });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[0]!);
  await userEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await waitFor(() => expect(tradesReply.resolve).toBeDefined());
  fireEvent.change(screen.getAllByLabelText("Time-shift, мин")[0]!, { target: { value: "7" } });
  expect(screen.queryByRole("button", { name: "Остановить" })).not.toBeInTheDocument();
  expect(screen.getAllByRole("button", { name: "Рассчитать" })).toHaveLength(2);
  expect(calls.filter((call) => call.path === "/v1/analytics/trades")).toHaveLength(1);
  expect(calls.filter((call) => call.path === "/v1/analytics/matrix-grid")).toHaveLength(1);
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await screen.findByText(current.setup.symbol);
  const oldQuery = new URLSearchParams(calls.find((call) => call.path === "/v1/analytics/matrix-grid")!.search);
  const cell = { ...matrixCell(4, 4, 9.99), ...(forecast ? { forecast, observationCutoff: Number(oldQuery.get("observationCutoff")), meanR: 0.8, forecastR: 9.99, decisionR: 9.99 } : {}) };
  await act(async () => {
    gridReply.resolve!(new Response(JSON.stringify({ items: [cell], ...selectionMetrics })));
    tradesReply.resolve!(new Response(JSON.stringify({ items: [stale], cachedCount: 1 })));
    await Promise.resolve();
  });
  expect(screen.queryByText("STALEUSDT")).not.toBeInTheDocument();
  expect(screen.queryByText("9.99")).not.toBeInTheDocument();
  expect(screen.getAllByLabelText("Time-shift, мин")[0]).toHaveValue("7");
  expect(calls.filter((call) => call.path === "/v1/analytics/trades")).toHaveLength(2);
  expect(calls.filter((call) => call.path === "/v1/analytics/matrix-grid")).toHaveLength(1);
});

it("keeps observed mean/sum local and independent across sum/mean/probability recipes, using genuine closed targets and per-trade M", async () => {
  vi.spyOn(Date, "now").mockReturnValue(probabilityCutoff * 1000);
  const rows = [utcTrade("2026-01-01T12:00:00Z", 2, 0), utcTrade("2026-01-01T13:00:00Z", 2, 1),
    utcTrade("2026-01-04T12:00:00Z", -1, 2), utcTrade("2026-01-06T12:00:00Z", null, 3),
    utcTrade("2026-01-10T12:00:00Z", 2, 4), utcTrade("2026-01-10T13:00:00Z", 2, 5)].map((row) => ({ ...row, tpPct: 2, slPct: 1 }));
  const calls = stubFetch({ ...utcChartOptions(rows), savedSettings: { trendMode: "blend", trendBlendLambda: "0.5",
    forecast: { method: "trend_sum_r", version: 1, bendDays: "linear" }, sections: { "short-trades": { tpInput: "2", slInput: "1" } } } });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  expect(calls.some((call) => call.method === "POST")).toBe(false);
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  const line = await screen.findByTestId("trend-decision-curve");
  const sumDecision = Number(line.getAttribute("data-decision-r"));
  const initial = JSON.parse(calls.find((call) => call.path === "/v1/analytics/trend")!.body!) as { points: TrendInput[]; projection: TrendProjectionRequest };
  expect(initial.points.map((point) => [point.value, point.weight])).toEqual([[4, 2], [-1, 1], [null, 1], [4, 2]]);
  expect(initial.projection).toEqual({ forecast: { method: "trend_sum_r", version: 1, bendDays: "linear" }, meanR: 1.4, trendMode: "blend", trendBlendLambda: 0.5 });
  expect(screen.getByLabelText("Фактический ряд по дням")).toHaveValue("mean");
  const initialCount = calls.length;
  fireEvent.change(screen.getByLabelText("Фактический ряд по дням"), { target: { value: "sum" } });
  expect(calls).toHaveLength(initialCount);
  expect(screen.getByTestId("trend-decision-curve")).toHaveAttribute("data-decision-r", String(sumDecision));
  const chart = screen.getByRole("img", { name: "Дневное матожидание в рисках" });
  expect(chart.querySelectorAll("circle")).toHaveLength(3);
  expect(chart.querySelector("circle title")).toHaveTextContent("сумма +4.00R, закрыто 2");
  fireEvent.change(screen.getByLabelText("Метод прогноза"), { target: { value: "trend" } });
  await waitFor(() => expect(calls.filter((call) => call.path === "/v1/analytics/trend")).toHaveLength(2));
  await screen.findByTestId("trend-decision-curve");
  const meanRequest = JSON.parse(calls.filter((call) => call.path === "/v1/analytics/trend").at(-1)!.body!) as typeof initial;
  expect(meanRequest.points.map((point) => [point.value, point.weight])).toEqual([[2, 2], [-1, 1], [null, 1], [2, 2]]);
  expect(meanRequest.projection.meanR).toBe(1.4);
  expect(Number(screen.getByTestId("trend-decision-curve").getAttribute("data-decision-r"))).not.toBe(sumDecision);
  expect(screen.getByLabelText("Фактический ряд по дням")).toHaveValue("sum");
  fireEvent.change(screen.getByLabelText("Метод прогноза"), { target: { value: "decayed_probability" } });
  await screen.findByTestId("forecast-model-curve");
  expect(screen.getByLabelText("Фактический ряд по дням")).toHaveValue("sum");
  const modelCalls = calls.filter((call) => call.method === "POST").length;
  fireEvent.change(screen.getByLabelText("Фактический ряд по дням"), { target: { value: "mean" } });
  expect(calls.filter((call) => call.method === "POST")).toHaveLength(modelCalls);
  expect(calls.filter((call) => call.path === "/v1/analytics/trades")).toHaveLength(1);
  expect(calls.some((call) => call.path.startsWith("/v1/analytics/trades/"))).toBe(false);
});

it("does not let a pending model response reset the observed selection or refetch the model", async () => {
  vi.spyOn(Date, "now").mockReturnValue(probabilityCutoff * 1000);
  const pending: { request?: ForecastCurveRequest; signal?: AbortSignal | undefined; resolve?: (response: Response) => void } = {};
  const calls = stubFetch({ ...probabilityOptions(), forecast: (request, signal) => {
    pending.request = request; pending.signal = signal;
    return new Promise<Response>((resolve) => { pending.resolve = resolve; });
  } });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await waitFor(() => expect(pending.resolve).toBeDefined());
  fireEvent.change(screen.getByLabelText("Фактический ряд по дням"), { target: { value: "sum" } });
  expect(pending.signal?.aborted).toBe(false);
  await act(async () => { pending.resolve!(new Response(JSON.stringify(forecastFixture(pending.request!)))); await Promise.resolve(); });
  await screen.findByTestId("forecast-model-curve");
  expect(screen.getByLabelText("Фактический ряд по дням")).toHaveValue("sum");
  expect(screen.getByRole("img").querySelector("circle title")).toHaveTextContent("сумма 4 R");
  expect(calls.filter((call) => call.path === "/v1/analytics/forecast")).toHaveLength(1);
});

it.each(["trend", "trend_sum_r"] as const)("retains a post-cutoff shifted-entry gate rejection as null setup weight in actual %s list assembly", async (method) => {
  const cutoff = Date.parse("2026-01-03T12:00:00Z") / 1000;
  vi.spyOn(Date, "now").mockReturnValue(cutoff * 1000);
  const rows = [utcTrade("2026-01-01T12:00:00Z", 1, 0), utcTrade("2026-01-02T12:00:00Z", -1, 1),
    utcTrade("2026-01-03T11:50:00Z", 1, 2), { ...utcTrade("2026-01-03T11:59:00Z", null, 3), status: "price_change_filtered" }]
    .map((row) => ({ ...row, closeTimeUnix: row.status === "price_change_filtered" ? null : row.setup.closeTimeUnix + 300 }));
  const calls = stubFetch({ ...utcChartOptions(rows),
    sets: [{ ...setFixture, fromUnix: rows[0]!.setup.closeTimeUnix, toUnix: rows.at(-1)!.setup.closeTimeUnix }],
    ...(method === "trend_sum_r" ? { interceptTradeRows: () => Promise.resolve(new Response(JSON.stringify({ items: [], cachedCount: 0 }))) } : {}),
    savedSettings: { trendMode: "forecast", forecast: { method, version: 1, bendDays: "linear" },
    sections: { "short-matrix": { timeShiftInput: "2", considerPriceChange: true, priceChangeInput: "0" } } } });
  render(<AnalyticsPage />); await screen.findByText("#12 · Утренний прогон");
  await userEvent.click(screen.getAllByRole("button", { name: "Рассчитать" })[1]!);
  await screen.findByTestId("trend-decision-curve");
  const request = JSON.parse(calls.find((call) => call.path === "/v1/analytics/trend")!.body!) as { points: TrendInput[]; projection: TrendProjectionRequest };
  expect(request.points.map((point) => [point.value, point.weight])).toEqual([[1, 1], [-1, 1], [1, 2]]);
  expect(request.projection.meanR).toBeCloseTo(1 / 3);
  expect(Number(screen.getByTestId("trend-decision-curve").getAttribute("data-decision-r"))).toBeCloseTo(7 / 11, 12);
  expect(screen.queryByText(rows[3]!.setup.symbol)).not.toBeInTheDocument();
  expect(screen.getByText("Отфильтровано смещением цены: 1")).toBeInTheDocument();
  expect(calls.filter((call) => call.path === "/v1/analytics/trades")).toHaveLength(1);
  expect(calls.filter((call) => call.path.startsWith("/v1/analytics/trades/"))).toHaveLength(method === "trend_sum_r" ? 4 : 0);
});

it("coalesces shared Setups forecast commits into one replacement matrix run per direction, with no list autorun", async () => {
  const calls = stubFetch({ shortBulk: fullShortGrid, longBulk: fullLongGrid, savedSettings: { autoCalc: true, trendMode: "forecast",
    forecast: { method: "trend", version: 1, bendDays: "linear" } } });
  render(<><AnalyticsPage /><SharedForecastControls /></>);
  await screen.findByText("#12 · Утренний прогон");
  const gridCalls = () => calls.filter((call) => call.path === "/v1/analytics/matrix-grid");
  await waitFor(() => expect(gridCalls()).toHaveLength(2));
  const shared = within(screen.getByRole("region", { name: "Общий прогноз Analytics и Сетапов" }));
  fireEvent.change(shared.getByLabelText("Метод прогноза"), { target: { value: "trend_sum_r" } });
  await waitFor(() => expect(gridCalls()).toHaveLength(4), { timeout: 1500 });
  fireEvent.change(shared.getByLabelText("Перегиб на N дней"), { target: { value: "7" } });
  await waitFor(() => expect(gridCalls()).toHaveLength(6), { timeout: 1500 });
  fireEvent.click(shared.getByRole("button", { name: "Смесь" }));
  await waitFor(() => expect(gridCalls()).toHaveLength(8), { timeout: 1500 });
  fireEvent.change(shared.getByLabelText("Доля прогноза λ"), { target: { value: "0.4" } });
  await waitFor(() => expect(gridCalls()).toHaveLength(10), { timeout: 1500 });
  // A local Analytics commit must not add a second run through the shared observer.
  const local = within(screen.getByRole("region", { name: "Аналитика" }));
  fireEvent.change(local.getByLabelText("Доля прогноза λ"), { target: { value: "0.6" } });
  await waitFor(() => expect(gridCalls()).toHaveLength(12), { timeout: 1500 });
  expect(calls.some((call) => call.path === "/v1/analytics/setups" || call.path.includes("/trades") || call.method === "POST")).toBe(false);
}, 15_000);

it("keeps shared Setups forecast edits manual when Analytics auto-calc is off", async () => {
  const calls = stubFetch({ savedSettings: { autoCalc: false, trendMode: "forecast", forecast: { method: "trend", version: 1, bendDays: "linear" } } });
  render(<><AnalyticsPage /><SharedForecastControls /></>); await screen.findByText("#12 · Утренний прогон");
  const shared = within(screen.getByRole("region", { name: "Общий прогноз Analytics и Сетапов" }));
  fireEvent.change(shared.getByLabelText("Метод прогноза"), { target: { value: "trend_sum_r" } });
  fireEvent.change(shared.getByLabelText("Перегиб на N дней"), { target: { value: "7" } });
  fireEvent.click(shared.getByRole("button", { name: "Смесь" }));
  fireEvent.change(shared.getByLabelText("Доля прогноза λ"), { target: { value: "0.4" } });
  await act(async () => { await new Promise((resolve) => { setTimeout(resolve, 950); }); });
  expect(calls.some((call) => call.path.includes("/matrix") || call.path === "/v1/analytics/setups" || call.path.includes("/trades") || call.method === "POST")).toBe(false);
});
