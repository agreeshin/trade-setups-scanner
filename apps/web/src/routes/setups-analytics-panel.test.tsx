import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { applyInterfaceSettings, DEFAULT_INTERFACE_SETTINGS } from "../lib/datetime-format";
import { SetupsAnalyticsPanel } from "./setups-analytics-panel";
import { forecastViewFixture } from "../../test/setups-forecast-fixture";
import type { SetupsProjectionRequest } from "../lib/setups-projection-api";
import { AnalyticsPage } from "./analytics-page";
import { RouteActiveContext } from "./route-active";

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });
const studyMeta1 = { studyId: 40, externalName: "Всплеск цены и оборота", setupCount: 3, generationId: 36, artifactId: 900 };
const studyMeta2 = { studyId: 41, externalName: "Всплеск цены и оборота", setupCount: 2, generationId: 37, artifactId: null };
const setupRow = (id: number, slotId: number, status: string, riskReturn: number | null) => ({
  id, symbol: `SYM${id}`, exchange: "bybit", slotId, studyId: 40, direction: "short",
  setupTimeUnix: 1794000000, entryTimeUnix: 1794000300, entryPrice: 98.5, gateThreshold: 98,
  tpPrice: 78.8, slPrice: 104.41, closeTimeUnix: 1794003600, riskReturn, status,
});
const analyticsResult = {
  totals: { selected: 5, gatePassed: 3, closed: 2, meanR: 1.25 },
  studies: [{ studyId: 40, selected: 5, gatePassed: 3, closed: 2, meanR: 1.25 }],
  setups: [
    setupRow(1, 1, "tp", 3.33), setupRow(2, 1, "sl", -1), setupRow(3, 3, "unclosed", null),
    setupRow(4, 1, "no_data", null), setupRow(5, 3, "gate_failed", null),
  ],
};
const optimizationStudy40 = {
  id: 40, status: "completed",
  spec: {
    algorithmId: "price_volume_spike", algorithmVersion: "1", algorithmParams: {}, algorithmSearch: {},
    analyticsParams: { clusterEnabled: true, clusterGapMinutes: 480, timeShiftMinutes: 5, priceChangePct: 2 }, analyticsSearch: {},
    instruments: [], fromUnix: 0, toUnix: 86400, direction: "short", setupMaturity: 0, horizonMinutes: 10080,
    tpLevels: [20], slLevels: [6], minClosedPerCell: 1, seed: 1, maxTrials: 10, maxSeconds: 60, trendMode: "blend",
  },
  progress: { phase: "done", evaluatedCandidates: 5, scorableCandidates: 3, maxTrials: 10, elapsedSeconds: 10, bestScore: 1.5 },
  result: {
    progress: { phase: "done", evaluatedCandidates: 5, scorableCandidates: 3, maxTrials: 10, elapsedSeconds: 10, bestScore: 1.5 },
    best: {
      score: 1.5, meanScore: 0.3, trendMode: "blend", partial: false,
      params: { spike_mult: 2.5, min_quote_vol: 1000 },
      analytics: { clusterEnabled: true, clusterGapMinutes: 480, timeShiftMinutes: 5, priceChangePct: 2, setupMaturity: 0.4, horizonMinutes: 10080 },
      matrix: [{ tpPct: 20, slPct: 6, riskExpectancy: 0.9, avgMovePct: 1, closedCount: 3, tpCount: 2, slCount: 1, unclosedCount: 0, noDataCount: 0, priceChangeFilteredCount: 0, meanRiskExpectancy: 0.4, forecastRiskExpectancy: 0.44 }],
      support: null,
      provenance: { optimizerVersion: "v", seed: 1, snapshotHash: "h", capturedAt: "2026-11-01T00:00:00Z" },
      conclusions: [],
    },
    reason: "trial_budget",
  },
  error: null,
  forecastBest: { forecastR: 0.44, meanR: 0.3, tpPct: 20, slPct: 6 },
  createdAt: "2026-11-06T00:00:00Z", updatedAt: "2026-11-07T00:00:00Z", events: [],
};
const optimizationStudy35 = {
  ...optimizationStudy40,
  id: 35,
  result: {
    ...optimizationStudy40.result,
    best: {
      ...optimizationStudy40.result.best,
      analytics: { clusterEnabled: false, clusterGapMinutes: null, timeShiftMinutes: 14, priceChangePct: 9, setupMaturity: null, horizonMinutes: 10080 },
    },
  },
};
const legacyAnalyticsResult = {
  ...analyticsResult,
  studies: [{ studyId: 35, selected: 5, gatePassed: 3, closed: 2, meanR: 1.25 }],
  setups: analyticsResult.setups.map((row) => ({ ...row, studyId: 35 })),
};

const requests: string[] = [];
let analyticsResponse: () => Response | Promise<Response> = () => json(analyticsResult);
let settingsResponse: (init?: RequestInit) => Response | Promise<Response>;
let metadataResponse: () => Response;
let projectionResponse: (init: RequestInit) => Response | Promise<Response>;
let studyResponse = () => json(optimizationStudy40);
let expectancyResponse = () => {
  const horizonDays = Array.from({ length: 13 }, (_, index) => 1793923200 + index * 86400);
  return json({ studyId: 40, observationCutoff: 1794009600, meanR: 0.2, daily: horizonDays.map((dayUnix, index) => ({ dayUnix, meanR: index % 2 === 0 ? 0.3 : -0.1, tpCount: 1, slCount: 1 })), trend: { method: "mean", points: [{ dayUnix: horizonDays[0]!, value: 0.2 }, { dayUnix: horizonDays.at(-1)!, value: 0.2 }] } });
};
const defaultExpectancyResponse = expectancyResponse;

beforeEach(() => {
  vi.useFakeTimers(); vi.setSystemTime(new Date("2026-11-13T12:00:00Z")); requests.length = 0;
  analyticsResponse = () => json(analyticsResult);
  settingsResponse = () => json({ settings: null });
  metadataResponse = () => json({ studies: [studyMeta1, studyMeta2] });
  projectionResponse = () => json({ error: "no_projection_fixture" }, 503);
  studyResponse = () => json(optimizationStudy40);
  expectancyResponse = defaultExpectancyResponse;
  window.localStorage.clear();
  applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "UTC" });
  Object.defineProperty(URL, "createObjectURL", { configurable: true, writable: true, value: vi.fn(() => "blob:expectancy-png") });
  Object.defineProperty(URL, "revokeObjectURL", { configurable: true, writable: true, value: vi.fn() });
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    requests.push(path);
    if (path === "/v1/algorithms") return Promise.resolve(json({ algorithms: [{ id: "price_volume_spike", name: "Рост цены и объёма", version: "1", description: "", enabled: true, params: {}, fields: [{ key: "spike_mult", label: "Множитель объёма", type: "decimal", min: 1, max: 20, unit: "×" }, { key: "min_quote_vol", label: "Мин. объём котировки", type: "decimal", min: 0, max: 1000000000, unit: "USDT" }] }] }));
    if (path === "/v1/settings/ui") return Promise.resolve(json({ value: {} }));
    if (path === "/v1/settings/web-analytics") return Promise.resolve(settingsResponse(init));
    if (path.startsWith("/v1/setups?")) return Promise.resolve(json({ setups: [], offset: Number(new URL(path, "http://fixture").searchParams.get("offset")), limit: 25, total: 0, snapshotId: 0 }));
    if (path.startsWith("/v1/setups/analytics/studies")) return Promise.resolve(metadataResponse());
    if (path.startsWith("/v1/setups/analytics/forecast-view?")) return Promise.resolve(projectionResponse(init!));
    if (path === "/v1/setups/analytics/study-expectancy/40") {
      return Promise.resolve(expectancyResponse());
    }
    if (path === "/v1/optimization-studies/40") return Promise.resolve(studyResponse());
    if (path === "/v1/optimization-studies/35") return Promise.resolve(json(optimizationStudy35));
    if (path === "/v1/setups/analytics/study-expectancy/35") {
      const horizonDays = Array.from({ length: 13 }, (_, index) => 1793923200 + index * 86400);
      return Promise.resolve(json({ studyId: 35, observationCutoff: 1794009600, meanR: 0.2, daily: horizonDays.map((dayUnix, index) => ({ dayUnix, meanR: index % 2 === 0 ? 0.3 : -0.1, tpCount: 1, slCount: 1 })), trend: { method: "mean", points: [{ dayUnix: horizonDays[0]!, value: 0.2 }, { dayUnix: horizonDays.at(-1)!, value: 0.2 }] } }));
    }
    if (path.startsWith("/v1/setups/analytics?")) return Promise.resolve(analyticsResponse());
    return Promise.resolve(json({}));
  }));
});
afterEach(() => { cleanup(); applyInterfaceSettings(DEFAULT_INTERFACE_SETTINGS); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

const settle = async () => { await act(async () => { await vi.advanceTimersByTimeAsync(0); }); };
const analyticsCalls = () => requests.filter((path) => path.startsWith("/v1/setups/analytics?"));
const studiesCalls = () => requests.filter((path) => path.startsWith("/v1/setups/analytics/studies"));
const unix = (iso: string) => Date.parse(iso) / 1000;
const storeRange = (from: string, to: string) => window.localStorage.setItem("setups-analytics-filter-v1", JSON.stringify({ algorithmId: "price_volume_spike", from, to, studies: null }));
const rowAt = (id: number, iso: string, status: string, riskReturn: number | null) => ({ ...setupRow(id, 1, status, riskReturn), setupTimeUnix: unix(iso), entryTimeUnix: unix(iso) });
const gridDays = (chart: HTMLElement) => [...chart.querySelectorAll("[data-grid-day]")].map((tick) => Number(tick.getAttribute("data-grid-day")));

async function rendered() {
  render(<SetupsAnalyticsPanel />);
  await settle();
  await settle();
  return null;
}

it("forwards literal search to fresh options, the mathematical chart cohort and paged history without automatically repeating POST", async () => {
  render(<SetupsAnalyticsPanel initialLoad={{ loadedAtUnix: unix("2026-11-13T12:00:37Z"), interfaceReady: true, ready: true }} />);
  await settle(); await settle();
  expect(analyticsCalls()).toHaveLength(1);
  expect(new URL(analyticsCalls()[0]!, "http://fixture").searchParams.get("toUnix")).toBe(String(unix("2026-11-13T12:00:37Z")));
  analyticsResponse = () => json({ totals: { selected: 1, gatePassed: 1, closed: 1, meanR: 2 }, studies: [], setups: [setupRow(9, 1, "tp", 2)] });
  fireEvent.change(screen.getByLabelText("Инструмент, биржа или алгоритм"), { target: { value: "%_" } });
  await settle();
  expect(analyticsCalls()).toHaveLength(1);
  expect(new URL(studiesCalls().at(-1)!, "http://fixture").searchParams.get("q")).toBe("%_");
  expect(new URL(requests.filter((path) => path.startsWith("/v1/setups?")).at(-1)!, "http://fixture").searchParams.get("q")).toBe("%_");
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" })); await settle();
  expect(analyticsCalls()).toHaveLength(2);
  expect(new URL(analyticsCalls().at(-1)!, "http://fixture").searchParams.get("q")).toBe("%_");
  expect(screen.getByTestId("setups-analytics-mean")).toHaveTextContent("+2.00R");
  expect(screen.getByTestId("setups-daily-outcomes").querySelector('circle[data-closed="1"]')).toHaveAttribute("data-mean-r", "2.00");
});

it("preserves clean committed instants and a mounted result on timezone changes, but does not reinterpret dirty edits", async () => {
  render(<SetupsAnalyticsPanel initialLoad={{ loadedAtUnix: unix("2026-11-13T12:00:00Z"), interfaceReady: true, ready: true }} />);
  await settle(); await settle();
  const chart = screen.getByTestId("setups-daily-outcomes");
  const end = chart.getAttribute("data-to-unix");
  act(() => { applyInterfaceSettings({ timezone: "Asia/Yekaterinburg" }); }); await settle();
  expect(screen.getByLabelText("Конец")).toHaveValue("2026-11-13T17:00");
  expect(screen.getByTestId("setups-daily-outcomes")).toBe(chart); expect(chart).toHaveAttribute("data-to-unix", end);
  expect(analyticsCalls()).toHaveLength(1);
  fireEvent.change(screen.getByLabelText("Начало (момент выхода сетапа)"), { target: { value: "2026-11-07T14:15" } });
  act(() => { applyInterfaceSettings({ timezone: "UTC" }); }); await settle();
  expect(screen.getByLabelText("Начало (момент выхода сетапа)")).toHaveValue("2026-11-07T00:00");
  expect(screen.getByRole("alert")).toHaveTextContent("при редактировании");
  expect(screen.getByRole("button", { name: "Рассчитать" })).toBeDisabled();
  expect(analyticsCalls()).toHaveLength(1);
});

it("preserves an explicit study subset across manual outcome-refresh readiness changes without another automatic calculation", async () => {
  const initial = { loadedAtUnix: unix("2026-11-13T12:00:00Z"), interfaceReady: true, ready: true };
  const view = render(<SetupsAnalyticsPanel initialLoad={initial} />); await settle(); await settle();
  fireEvent.click(screen.getByRole("button", { name: "Исследования: Все (2)" }));
  fireEvent.click(screen.getByRole("checkbox", { name: "Исследование 41" }));
  fireEvent.click(screen.getByRole("button", { name: "Готово" })); await settle();
  expect(screen.getByRole("button", { name: "Исследования: 1 из 2" })).toBeInTheDocument();
  view.rerender(<SetupsAnalyticsPanel initialLoad={{ ...initial, ready: false }} />); await settle();
  view.rerender(<SetupsAnalyticsPanel initialLoad={initial} />); await settle();
  expect(screen.getByRole("button", { name: "Исследования: 1 из 2" })).toBeInTheDocument();
  expect(analyticsCalls()).toHaveLength(1);
  const lastHistory = requests.filter((path) => path.startsWith("/v1/setups?")).at(-1)!;
  expect(new URL(lastHistory, "http://fixture").searchParams.get("studies")).toBe("40");
});

it("waits after a study-options failure and continues the sole initial calculation only after explicit retry", async () => {
  const savedFetch = globalThis.fetch;
  let failed = false;
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    if (path.startsWith("/v1/setups/analytics/studies") && !failed) {
      failed = true;
      return Promise.resolve(json({ error: "unavailable" }, 503));
    }
    return savedFetch(path, init);
  }));
  render(<SetupsAnalyticsPanel initialLoad={{ loadedAtUnix: unix("2026-11-13T12:00:00Z"), interfaceReady: true, ready: true }} />);
  await settle(); await settle();
  expect(analyticsCalls()).toHaveLength(0);
  fireEvent.click(screen.getByRole("button", { name: "Повторить загрузку исследований" })); await settle();
  expect(analyticsCalls()).toHaveLength(1);
});

it("loads studies for the committed range and renders the calculated result", async () => {
  await rendered();
  expect(requests).toContain("/v1/algorithms");
  expect(studiesCalls()).toHaveLength(1);
  expect(screen.getByRole("button", { name: "Исследования: Все (2)" })).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  expect(analyticsCalls()).toHaveLength(1);
  const mean = screen.getByTestId("setups-analytics-mean");
  expect(mean.className).toContain("text-success");
  expect(screen.getAllByText("3/5")).toHaveLength(2);
  expect(screen.getByRole("heading", { name: "Исследования-основы" })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Подробности исследования 40" })).toBeInTheDocument();
  expect(screen.getByText("2/3")).toBeInTheDocument();
  const chart = screen.getByTestId("setups-daily-outcomes");
  const dayPoint = chart.querySelector("circle[data-closed='2']");
  expect(dayPoint).not.toBeNull();
  expect(Number(dayPoint!.getAttribute("data-mean-r"))).toBeCloseTo(1.17, 1);
  expect(dayPoint!.getAttribute("data-day")).not.toBe("");
  expect(screen.getByRole("heading", { name: /Сетапы/ })).toBeInTheDocument();
  expect(screen.getByText("SYM1")).toBeInTheDocument();
  expect(screen.getAllByText("TP")).toHaveLength(2);
  expect(screen.getAllByText("SL")).toHaveLength(2);
  expect(screen.getByText("Не закрылась")).toBeInTheDocument();
  expect(screen.getByText("Нет данных")).toBeInTheDocument();
  expect(screen.getByText("4/5")).toBeInTheDocument();
  expect(screen.queryByText("Гейт не пройден")).toBeNull();
  expect(screen.queryByText("SYM5")).toBeNull();
  expect(screen.getAllByText("№40").length).toBe(4);
  fireEvent.click(screen.getByLabelText("Скрыть сетапы, которые не прошли гейт"));
  expect(screen.getByText("Гейт не пройден")).toBeInTheDocument();
  expect(screen.getByText("SYM5")).toBeInTheDocument();
  expect(screen.getByText("5/5")).toBeInTheDocument();
});

it("opens the picker, edits the study selection and passes studies to the calculation", async () => {
  await rendered();
  fireEvent.click(screen.getByRole("button", { name: "Исследования: Все (2)" }));
  await settle();
  const dialog = screen.getByRole("dialog");
  expect(within(dialog).getByText("Исследования-основы, выпустившие сетапы")).toBeInTheDocument();
  expect(within(dialog).getByText("Выбрано: 2 из 2")).toBeInTheDocument();
  fireEvent.click(within(dialog).getByLabelText("Исследование 41"));
  expect(within(dialog).getByText("Выбрано: 1 из 2")).toBeInTheDocument();
  fireEvent.click(within(dialog).getByRole("button", { name: "Готово" }));
  await settle();
  expect(screen.getByRole("button", { name: "Исследования: 1 из 2" })).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  const query = new URL(analyticsCalls().at(-1)!, "http://localhost").searchParams;
  expect(query.get("studies")).toBe("40");
  expect(query.get("algorithmId")).toBe("price_volume_spike");
});

it("reloads the study list on date blur and resets the selection to all", async () => {
  await rendered();
  fireEvent.click(screen.getByRole("button", { name: "Исследования: Все (2)" }));
  await settle();
  const dialog = screen.getByRole("dialog");
  fireEvent.click(within(dialog).getByLabelText("Исследование 41"));
  fireEvent.click(within(dialog).getByRole("button", { name: "Готово" }));
  await settle();
  expect(screen.getByRole("button", { name: "Исследования: 1 из 2" })).toBeInTheDocument();
  fireEvent.change(screen.getByLabelText("Начало (момент выхода сетапа)"), { target: { value: "2026-11-07T00:00" } });
  await settle();
  fireEvent.blur(screen.getByLabelText("Начало (момент выхода сетапа)"));
  await settle();
  await settle();
  expect(studiesCalls()).toHaveLength(2);
  expect(screen.getByRole("button", { name: "Исследования: Все (2)" })).toBeInTheDocument();
});

it("restores persisted filters and selection from local storage", async () => {
  window.localStorage.setItem("setups-analytics-filter-v1", JSON.stringify({
    algorithmId: "price_volume_spike", from: "2026-11-06T12:00", to: "2026-11-13T12:00", studies: [40],
  }));
  await rendered();
  expect(screen.getByRole("button", { name: "Исследования: 1 из 2" })).toBeInTheDocument();
  expect(screen.getByLabelText("Начало (момент выхода сетапа)")).toHaveValue("2026-11-06T12:00");
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  const query = new URL(analyticsCalls().at(-1)!, "http://localhost").searchParams;
  expect(query.get("studies")).toBe("40");
});

it("shows study details with the past-and-live chart in a modal", async () => {
  await rendered();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  fireEvent.click(screen.getByRole("button", { name: "Подробности исследования 40" }));
  await settle();
  await settle();
  const dialog = screen.getByRole("dialog");
  expect(within(dialog).getByText("Исследование №40")).toBeInTheDocument();
  expect(within(dialog).getByText("Лучший score")).toBeInTheDocument();
  expect(within(dialog).getByText("+1.50 R")).toBeInTheDocument();
  expect(within(dialog).getByText("Лучшая blend-пара")).toBeInTheDocument();
  expect(within(dialog).getByText("TP 20% / SL 6% · +0.44 R")).toBeInTheDocument();
  expect(within(dialog).getByText("Полные параметры алгоритма")).toBeInTheDocument();
  expect(within(dialog).getByText("Множитель объёма (×)")).toBeInTheDocument();
  expect(within(dialog).getByText("Мин. объём котировки (USDT)")).toBeInTheDocument();
  expect(within(dialog).getAllByText("2.5").length).toBeGreaterThan(0);
  expect(within(dialog).getByText("Аналитические параметры")).toBeInTheDocument();
  expect(within(dialog).getByText("Сдвиг времени, мин")).toBeInTheDocument();
  expect(within(dialog).getByText("Смещение цены, %")).toBeInTheDocument();
  expect(within(dialog).getByText("включена")).toBeInTheDocument();
  const chart = within(dialog).getByTestId("study-horizon-chart");
  expect(chart.querySelector("[data-divider-day]")).not.toBeNull();
  expect(chart.querySelectorAll(".grid-day").length).toBe(13);
  expect(chart.querySelectorAll(".date").length).toBeGreaterThanOrEqual(2);
  expect(chart.querySelectorAll('[data-phase="past"]').length).toBe(12);
  expect(chart.querySelectorAll('[data-phase="release"]').length).toBe(1);
  expect(chart.querySelectorAll('[data-phase="live"]').length).toBe(0);
  const releasePoint = chart.querySelector('[data-phase="release"]');
  expect(releasePoint!.getAttribute("fill")).toBe("#18cf98");
  expect(releasePoint!.getAttribute("data-mean-r")).toBe("0.73");
  const pastPoint = chart.querySelector('[data-phase="past"]');
  expect(pastPoint!.getAttribute("fill")).toMatch(/#18cf98|#f08a7f/);
  const line = chart.querySelector('[data-testid="expectancy-line"]');
  expect(line).not.toBeNull();
  const releaseCx = chart.querySelector('[data-phase="release"]')!.getAttribute("cx");
  expect(line!.getAttribute("points")).toContain(`${releaseCx},`);
  const forecast = chart.querySelector('[data-testid="forecast-line"]');
  expect(forecast).not.toBeNull();
  expect(forecast!.getAttribute("stroke-dasharray")).toBeNull();
  const nowValue = chart.querySelector('[data-testid="forecast-now-value"]');
  expect(nowValue).not.toBeNull();
  expect(nowValue!.getAttribute("fill")).toBe("#f5b041");
  expect(nowValue!.textContent).toBe("+0.20R");
  expect(dialog.querySelector("img")).toBeNull();
  expect(requests).toContain("/v1/optimization-studies/40");
  expect(requests.some((path) => path === "/v1/setups/analytics/study-expectancy/40")).toBe(true);
});

it("reports a busy live-outcome read and does not request a misleading historical-only combined chart", async () => {
  await rendered();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  analyticsResponse = () => json({ error: "setups_outcomes_busy" }, 409);
  fireEvent.click(screen.getByRole("button", { name: "Подробности исследования 40" }));
  await settle(); await settle();
  const dialog = within(screen.getByRole("dialog"));
  expect(requests.filter((path) => path === "/v1/setups/analytics/study-expectancy/40")).toHaveLength(0);
  expect(dialog.getByRole("alert")).toHaveTextContent("HTTP 409");
  expect(dialog.queryByTestId("study-horizon-chart")).not.toBeInTheDocument();
  expect(dialog.queryByText("Загрузка графика…")).not.toBeInTheDocument();
  expect(dialog.getByText("Полные параметры алгоритма")).toBeInTheDocument();
  expect(dialog.getByText("TP 20% / SL 6% · +0.44 R")).toBeInTheDocument();
});

it("accepts a successful empty live population and still requests the saved study series with live=[]", async () => {
  await rendered();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  analyticsResponse = () => json({ totals: { selected: 0, gatePassed: 0, closed: 0, meanR: null }, studies: [], setups: [] });
  fireEvent.click(screen.getByRole("button", { name: "Подробности исследования 40" }));
  await settle(); await settle();
  const dialog = within(screen.getByRole("dialog"));
  const seriesCalls = vi.mocked(globalThis.fetch).mock.calls.filter(([path]) => path === "/v1/setups/analytics/study-expectancy/40");
  expect(seriesCalls).toHaveLength(1);
  expect(seriesCalls[0]?.[1]?.body).toBe(JSON.stringify({ live: [] }));
  expect(dialog.queryByRole("alert")).not.toBeInTheDocument();
  const chart = dialog.getByTestId("study-horizon-chart");
  expect(chart.querySelectorAll('[data-phase="past"]').length).toBeGreaterThan(0);
  expect(chart.querySelectorAll('[data-phase="live"]')).toHaveLength(0);
  expect(dialog.getByText("TP 20% / SL 6% · +0.44 R")).toBeInTheDocument();
});

it("opens legacy study details with null analytics without white screen", async () => {
  analyticsResponse = () => json(legacyAnalyticsResult);
  await rendered();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  fireEvent.click(screen.getByRole("button", { name: "Подробности исследования 35" }));
  await settle();
  await settle();
  const dialog = screen.getByRole("dialog");
  expect(within(dialog).getByText("Исследование №35")).toBeInTheDocument();
  expect(within(dialog).getByText("выключена")).toBeInTheDocument();
  expect(within(dialog).getAllByText("—").length).toBeGreaterThanOrEqual(2);
  expect(within(dialog).getByTestId("study-horizon-chart")).toBeInTheDocument();
});

it("disables calculation and hints when the range is invalid", async () => {
  await rendered();
  fireEvent.change(screen.getByLabelText("Начало (момент выхода сетапа)"), { target: { value: "2026-11-20T00:00" } });
  await settle();
  expect(screen.getByRole("alert")).toHaveTextContent("конец диапазона должен быть позже начала");
  expect(screen.getByRole("button", { name: "Рассчитать" })).toBeDisabled();
  expect(analyticsCalls()).toHaveLength(0);
});

it("shows the limit error with a range hint", async () => {
  analyticsResponse = () => json({ error: "setups_analytics_limit_exceeded" }, 400);
  await rendered();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  expect(screen.getByRole("alert")).toHaveTextContent("Сузьте диапазон");
});

it("shows an operational candle-read failure instead of presenting missing-data results", async () => {
  analyticsResponse = () => json({ error: "setups_analytics_market_data_unavailable" }, 502);
  await rendered();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  expect(screen.getByRole("alert")).toHaveTextContent("Не удалось прочитать свечи из сервиса данных");
  expect(screen.getByRole("alert")).toHaveTextContent("Повторите расчёт после восстановления сервиса");
  expect(screen.queryByTestId("setups-analytics-mean")).toBeNull();
});

it("groups the 19th at 22Z into the configured UTC+5 day and includes the last partial day without a zero outcome", async () => {
  // Make the browser's reported zone different from the explicitly configured interface zone.
  const browserOptions = new Intl.DateTimeFormat().resolvedOptions();
  vi.spyOn(Intl.DateTimeFormat.prototype, "resolvedOptions").mockReturnValue({ ...browserOptions, timeZone: "UTC" });
  applyInterfaceSettings({ timezone: "Asia/Yekaterinburg" });
  vi.setSystemTime(new Date("2026-11-21T05:00:00Z"));
  storeRange("2026-11-19T00:00", "2026-11-21T10:00");
  analyticsResponse = () => json({ ...analyticsResult, setups: [
    rowAt(1, "2026-11-19T22:00:00Z", "tp", 3),
    rowAt(2, "2026-11-20T01:00:00Z", "sl", -1),
    rowAt(3, "2026-11-19T18:00:00Z", "tp", 0.5),
    rowAt(4, "2026-11-21T02:00:00Z", "unclosed", null),
  ] });
  await rendered();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  const query = new URL(analyticsCalls()[0]!, "http://localhost").searchParams;
  expect(Number(query.get("fromUnix"))).toBe(unix("2026-11-18T19:00:00Z"));
  expect(Number(query.get("toUnix"))).toBe(unix("2026-11-21T05:00:00Z"));
  const chart = screen.getByTestId("setups-daily-outcomes");
  expect(gridDays(chart)).toEqual(["2026-11-18T19:00:00Z", "2026-11-19T19:00:00Z", "2026-11-20T19:00:00Z"].map(unix));
  const local20 = chart.querySelector('circle[data-closed="2"]');
  expect(local20).toHaveAttribute("data-day", "20.11.2026");
  expect(local20).toHaveAttribute("data-mean-r", "1.00");
  expect(chart.querySelectorAll("circle")).toHaveLength(2);
  expect(chart.querySelector('circle[data-day="21.11.2026"]')).toBeNull();
  expect(within(chart).getByText("21.11.2026")).toBeInTheDocument();
  expect(screen.queryByText(/00:00 UTC/)).toBeNull();
});

it("keeps the selected calendar axis when there are no closed trades at all", async () => {
  applyInterfaceSettings({ timezone: "Asia/Yekaterinburg" });
  storeRange("2026-11-19T00:00", "2026-11-21T10:00");
  analyticsResponse = () => json({ ...analyticsResult, setups: [rowAt(1, "2026-11-19T22:00:00Z", "unclosed", null)] });
  await rendered();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  const chart = screen.getByTestId("setups-daily-outcomes");
  expect(gridDays(chart)).toHaveLength(3);
  expect(within(chart).getByText("21.11.2026")).toBeInTheDocument();
  expect(within(chart).getByText("Нет закрытых сделок для графика по дням.")).toBeInTheDocument();
  expect(chart.querySelectorAll("circle, polyline")).toHaveLength(0);
});

it("does not add an extra calendar day for an exclusive midnight range end", async () => {
  applyInterfaceSettings({ timezone: "Asia/Yekaterinburg" });
  storeRange("2026-11-19T00:00", "2026-11-21T00:00");
  analyticsResponse = () => json({ ...analyticsResult, setups: [rowAt(1, "2026-11-19T22:00:00Z", "tp", 1)] });
  await rendered();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  expect(gridDays(screen.getByTestId("setups-daily-outcomes"))).toEqual(["2026-11-18T19:00:00Z", "2026-11-19T19:00:00Z"].map(unix));
});

it("extends the selected axis to a shifted entry on the following local day without drawing outside the SVG", async () => {
  applyInterfaceSettings({ timezone: "Asia/Yekaterinburg" });
  storeRange("2026-11-19T00:00", "2026-11-20T23:58");
  analyticsResponse = () => json({ ...analyticsResult, setups: [
    { ...rowAt(1, "2026-11-20T19:08:00Z", "tp", 1), setupTimeUnix: unix("2026-11-20T18:58:00Z") },
  ] });
  await rendered();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  const chart = screen.getByTestId("setups-daily-outcomes");
  expect(gridDays(chart)).toEqual(["2026-11-18T19:00:00Z", "2026-11-19T19:00:00Z", "2026-11-20T19:00:00Z"].map(unix));
  expect(within(chart).getByText("20.11.2026")).toBeInTheDocument();
  const shiftedEntry = chart.querySelector('circle[data-day="21.11.2026"]');
  expect(shiftedEntry).toHaveAttribute("cx", "872");
  expect(shiftedEntry).toHaveAttribute("data-mean-r", "1.00");
});

it("drops a stale calculation if its input changes while the response is running", async () => {
  applyInterfaceSettings({ timezone: "Asia/Yekaterinburg" });
  storeRange("2026-11-19T00:00", "2026-11-21T10:00");
  let resolveResponse: ((value: Response) => void) | undefined;
  analyticsResponse = () => new Promise<Response>((resolve) => { resolveResponse = resolve; });
  await rendered();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  fireEvent.change(screen.getByLabelText("Конец"), { target: { value: "2026-11-22T00:00" } });
  act(() => { resolveResponse!(json({ ...analyticsResult, setups: [rowAt(1, "2026-11-19T22:00:00Z", "tp", 1)] })); });
  await settle();
  expect(screen.queryByTestId("setups-daily-outcomes")).toBeNull();
  expect(screen.getByLabelText("Конец")).toHaveValue("2026-11-22T00:00");
});

it("iterates local calendar midnights through DST instead of adding 86400 seconds", async () => {
  applyInterfaceSettings({ timezone: "America/New_York" });
  storeRange("2026-03-07T00:00", "2026-03-10T12:00");
  analyticsResponse = () => json({ ...analyticsResult, setups: [rowAt(1, "2026-03-08T18:00:00Z", "tp", 1)] });
  await rendered();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  const chart = screen.getByTestId("setups-daily-outcomes");
  expect(gridDays(chart)).toEqual(["2026-03-07T05:00:00Z", "2026-03-08T05:00:00Z", "2026-03-09T04:00:00Z", "2026-03-10T04:00:00Z"].map(unix));
  expect([...chart.querySelectorAll("[data-grid-day] text")].map((label) => label.textContent)).toEqual(["07.03.2026", "08.03.2026", "09.03.2026", "10.03.2026"]);
});

it("redraws mounted daily outcomes on interface timezone changes without recalculating", async () => {
  applyInterfaceSettings({ timezone: "Asia/Yekaterinburg" });
  storeRange("2026-11-19T00:00", "2026-11-21T10:00");
  analyticsResponse = () => json({ ...analyticsResult, setups: [rowAt(1, "2026-11-19T22:00:00Z", "tp", 3), rowAt(2, "2026-11-20T01:00:00Z", "sl", -1)] });
  await rendered();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  const chart = screen.getByTestId("setups-daily-outcomes");
  expect(chart.querySelector('circle[data-closed="2"]')).toHaveAttribute("data-day", "20.11.2026");
  act(() => { applyInterfaceSettings({ timezone: "UTC" }); });
  await settle();
  expect(screen.getByTestId("setups-daily-outcomes")).toBe(chart);
  expect(chart.querySelector('circle[data-day="19.11.2026"]')).toHaveAttribute("data-mean-r", "3.00");
  expect(chart.querySelector('circle[data-day="20.11.2026"]')).toHaveAttribute("data-mean-r", "-1.00");
  expect(chart).toHaveAttribute("data-from-unix", String(unix("2026-11-18T19:00:00Z")));
  expect(analyticsCalls()).toHaveLength(1);
});

it("maps modal buckets and release continuity to the configured day but preserves exact forecast instants and UTC model inputs", async () => {
  applyInterfaceSettings({ timezone: "Asia/Yekaterinburg" });
  vi.setSystemTime(new Date("2026-11-21T12:00:00Z"));
  storeRange("2026-11-19T00:00", "2026-11-21T17:00");
  analyticsResponse = () => json({ ...analyticsResult, setups: [rowAt(1, "2026-11-19T22:00:00Z", "tp", 3), rowAt(2, "2026-11-20T01:00:00Z", "sl", -1)] });
  studyResponse = () => json({ ...optimizationStudy40, createdAt: "2026-11-19T22:00:00Z" });
  const trend = [
    { dayUnix: unix("2026-11-19T02:30:00Z"), value: 0.1 },
    { dayUnix: unix("2026-11-19T22:15:00Z"), value: 0.4 },
    { dayUnix: unix("2026-11-20T12:45:00Z"), value: 0.6 },
    { dayUnix: unix("2026-11-21T13:00:00Z"), value: 0.8 },
  ];
  expectancyResponse = () => json({ studyId: 40, observationCutoff: unix("2026-11-20T00:00:00Z"), meanR: 0.3, daily: [
    { dayUnix: unix("2026-11-19T00:00:00Z"), meanR: 0.4, tpCount: 1, slCount: 1 },
    { dayUnix: unix("2026-11-20T00:00:00Z"), meanR: 0.2, tpCount: 1, slCount: 1 },
  ], trend: { method: "probability", points: trend } });
  await rendered();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  fireEvent.click(screen.getByRole("button", { name: "Подробности исследования 40" }));
  await settle();
  await settle();
  const chart = within(screen.getByRole("dialog")).getByTestId("study-horizon-chart");
  expect(chart.querySelector("[data-divider-day]")).toHaveAttribute("data-divider-day", String(unix("2026-11-19T19:00:00Z")));
  expect(chart.querySelector('[data-phase="past"]')).toHaveAttribute("data-day", "19.11.2026");
  expect(chart.querySelector('[data-phase="past"]')).toHaveAttribute("data-mean-r", "0.40");
  expect(chart.querySelector('[data-phase="release"]')).toHaveAttribute("data-day", "20.11.2026");
  expect(chart.querySelector('[data-phase="release"]')).toHaveAttribute("data-closed", "4");
  expect(chart.querySelector('[data-phase="release"]')).toHaveAttribute("data-mean-r", "0.60");
  const releaseX = chart.querySelector('[data-phase="release"]')!.getAttribute("cx");
  expect(chart.querySelector('[data-testid="expectancy-line"]')!.getAttribute("points")).toContain(`${releaseX},`);
  const firstDay = unix("2026-11-18T19:00:00Z");
  const now = unix("2026-11-21T12:00:00Z");
  const expectedX = (instant: number) => 80 + (instant - firstDay) / (now - firstDay) * (872 - 80);
  const forecast = chart.querySelector('[data-testid="forecast-line"]')!;
  const forecastX = forecast.getAttribute("points")!.split(" ").map((point) => Number(point.split(",")[0]));
  expect(forecastX).toHaveLength(4);
  [...trend.slice(0, 3).map((point) => point.dayUnix), now].forEach((instant, index) => expect(forecastX[index]).toBeCloseTo(expectedX(instant), 7));
  expect(forecast).not.toHaveAttribute("stroke-dasharray");
  expect(forecast).toHaveAttribute("stroke", "#f5b041");
  expect(chart.querySelector('[data-testid="forecast-now-value"]')).toHaveAttribute("x", "872");
  expect(chart.querySelector('[data-testid="forecast-now-value"]')).toHaveTextContent("+0.79R");
  const modelCalls = vi.mocked(fetch).mock.calls.filter(([path]) => path === "/v1/setups/analytics/study-expectancy/40");
  expect(modelCalls).toHaveLength(1);
  expect(JSON.parse(modelCalls[0]![1]!.body as string)).toEqual({ live: [
    { dayUnix: unix("2026-11-19T00:00:00Z"), meanR: 3, closedCount: 1 },
    { dayUnix: unix("2026-11-20T00:00:00Z"), meanR: -1, closedCount: 1 },
  ] });
  const callsBeforeChange = analyticsCalls().length;
  vi.setSystemTime(new Date("2026-11-22T12:00:00Z"));
  act(() => { applyInterfaceSettings({ timezone: "America/Los_Angeles" }); });
  await settle();
  expect(within(screen.getByRole("dialog")).getByTestId("study-horizon-chart")).toBe(chart);
  // Original UTC midnight on the 19th maps to the 18th, not to local midnight of the 19th.
  expect(chart.querySelector('[data-phase="past"]')).toHaveAttribute("data-day", "18.11.2026");
  expect(chart.querySelector('[data-phase="past"]')).toHaveAttribute("data-mean-r", "0.40");
  expect(chart.querySelector('[data-phase="past"]')).toHaveAttribute("data-closed", "2");
  expect(chart.querySelector("[data-divider-day]")).toHaveAttribute("data-divider-day", String(unix("2026-11-19T08:00:00Z")));
  expect(chart.querySelector('[data-phase="release"]')).toHaveAttribute("data-day", "19.11.2026");
  expect(chart.querySelector('[data-phase="release"]')).toHaveAttribute("data-mean-r", "0.60");
  expect(chart.querySelector('[data-testid="forecast-now-value"]')).toHaveTextContent("+0.79R");
  expect(gridDays(chart).at(-1)).toBe(unix("2026-11-21T08:00:00Z"));
  expect(analyticsCalls()).toHaveLength(callsBeforeChange);
  expect(vi.mocked(fetch).mock.calls.filter(([path]) => path === "/v1/setups/analytics/study-expectancy/40")).toHaveLength(1);
});

type ForecastBody = Pick<SetupsProjectionRequest, "snapshot" | "forecast" | "trendMode" | "trendBlendLambda" | "timeZone">;
const forecastBody = (init: RequestInit) => { if (typeof init.body !== "string") throw new Error("Expected JSON string"); return JSON.parse(init.body) as ForecastBody; };
const projectionCalls = () => vi.mocked(fetch).mock.calls.filter(([path]) => typeof path === "string" && path.startsWith("/v1/setups/analytics/forecast-view?"));
function enableSnapshotForecast() {
  const snapshot = { snapshotId: 5, observationCutoff: Math.floor(Date.now() / 60000) * 60, fingerprint: "a".repeat(64) };
  analyticsResponse = () => json({ ...analyticsResult, snapshot });
  settingsResponse = () => json({ settings: { trendMode: "forecast", trendBendDays: "linear", forecast: { method: "trend", version: 1, bendDays: "linear" } } });
  projectionResponse = (init) => json(forecastViewFixture({ ...forecastBody(init), expectedSelected: 5 }));
  return snapshot;
}

it("renders the server M/F/B projection from the full calculated snapshot independently of empty visible history, without changing End or refreshing outcomes", async () => {
  const snapshot = enableSnapshotForecast();
  render(<SetupsAnalyticsPanel initialLoad={{ loadedAtUnix: Math.floor(Date.now() / 1000), interfaceReady: true, ready: true }} />);
  await settle(); await act(async () => { await vi.advanceTimersByTimeAsync(250); });
  const originalEnd = screen.getByLabelText<HTMLInputElement>("Конец").value;
  expect(analyticsCalls()).toHaveLength(1); expect(projectionCalls()).toHaveLength(1);
  expect(screen.getByText("Сетапы не найдены.")).toBeInTheDocument();
  expect(forecastBody(projectionCalls()[0]![1]!)).toMatchObject({ snapshot, trendMode: "forecast" });
  expect(new URL(projectionCalls()[0]![0] as string, "http://fixture").searchParams.has("offset")).toBe(false);
  expect(screen.getByTestId("setups-analytics-mean")).toHaveTextContent("+1.25R");
  expect(screen.getByTestId("setups-model-summary")).toHaveTextContent("+3.75R");
  expect(screen.getByText("Модель F · Тренд дневной суммы R")).toBeInTheDocument();
  expect(screen.getByTestId("setups-model-now-value")).toHaveTextContent("+3.75R");
  const marker = screen.getByTestId("setups-model-now-marker");
  expect(marker).toHaveAttribute("fill", "#f5b041"); expect(marker).toHaveAttribute("data-now-unix", String(snapshot.observationCutoff + 37));
  expect(screen.getByTestId("setups-model-now-value").getAttribute("x")).toBe(marker.getAttribute("cx"));
  for (const circle of screen.getByTestId("setups-daily-outcomes").querySelectorAll("circle")) {
    expect(Number(circle.getAttribute("cy"))).toBeGreaterThanOrEqual(66); expect(Number(circle.getAttribute("cy"))).toBeLessThanOrEqual(258);
  }
  fireEvent.click(screen.getByRole("button", { name: "Смесь" }));
  await act(async () => { await vi.advanceTimersByTimeAsync(250); });
  expect(projectionCalls()).toHaveLength(2); expect(forecastBody(projectionCalls()[1]![1]!)).toMatchObject({ snapshot, trendMode: "blend", trendBlendLambda: 0.7 });
  expect(screen.getByTestId("setups-model-summary")).toHaveTextContent("+3.00R");
  expect(screen.getByTestId("setups-model-now-value")).toHaveTextContent("+3.00R");
  expect(screen.getByTestId("setups-analytics-mean")).toHaveTextContent("+1.25R");
  await act(async () => { await vi.advanceTimersByTimeAsync(600000); });
  expect(screen.getByLabelText("Конец")).toHaveValue(originalEnd); expect(analyticsCalls()).toHaveLength(1); expect(projectionCalls()).toHaveLength(2);
  expect(requests.some((path) => /outcomes\/refresh|study-actual|replay|maintenance|optimization-studies/.test(path))).toBe(false);
});

it.each(["success", "failure"])("ignores an older projection's late %s after a newer control change and response", async (outcome) => {
  enableSnapshotForecast();
  const pending: { body: ForecastBody; resolve: (value: Response) => void }[] = [];
  projectionResponse = (init) => new Promise<Response>((resolve) => { pending.push({ body: forecastBody(init), resolve }); });
  render(<SetupsAnalyticsPanel initialLoad={{ loadedAtUnix: Math.floor(Date.now() / 1000), interfaceReady: true, ready: true }} />);
  await settle(); await act(async () => { await vi.advanceTimersByTimeAsync(250); });
  fireEvent.click(screen.getByRole("button", { name: "Смесь" }));
  expect(screen.getByTestId("setups-model-summary")).toHaveTextContent("—");
  await act(async () => { await vi.advanceTimersByTimeAsync(250); });
  expect(pending).toHaveLength(2);
  await act(async () => { pending[1]!.resolve(json(forecastViewFixture({ ...pending[1]!.body, expectedSelected: 5 }))); await Promise.resolve(); });
  expect(screen.getByTestId("setups-model-summary")).toHaveTextContent("+3.00R");
  await act(async () => { pending[0]!.resolve(outcome === "success" ? json(forecastViewFixture({ ...pending[0]!.body, expectedSelected: 5 })) : json({ error: "late" }, 503)); await Promise.resolve(); });
  expect(screen.getByTestId("setups-model-summary")).toHaveTextContent("+3.00R");
  expect(screen.getByTestId("setups-model-now-value")).toHaveTextContent("+3.00R");
  expect(screen.queryByText(/HTTP 503/)).not.toBeInTheDocument(); expect(analyticsCalls()).toHaveLength(1);
});

it("keeps a changed/unavailable snapshot explicit with no zero/old curve and no implicit retry", async () => {
  enableSnapshotForecast(); projectionResponse = () => json({ error: "setups_analytics_snapshot_changed" }, 409);
  render(<SetupsAnalyticsPanel initialLoad={{ loadedAtUnix: Math.floor(Date.now() / 1000), interfaceReady: true, ready: true }} />);
  await settle(); await act(async () => { await vi.advanceTimersByTimeAsync(250); });
  expect(screen.getByText(/Сохранённые исходы снимка изменились/)).toBeInTheDocument();
  expect(screen.getByTestId("setups-model-summary")).toHaveTextContent("—"); expect(screen.queryByTestId("setups-model-line")).toBeNull();
  expect(screen.getByTestId("setups-analytics-mean")).toHaveTextContent("+1.25R");
  await act(async () => { await vi.advanceTimersByTimeAsync(5000); });
  expect(projectionCalls()).toHaveLength(1); expect(analyticsCalls()).toHaveLength(1);
});

it("shows the HTTP 500 model error with exactly one presentation prefix while keeping prior results and never retrying or replaying", async () => {
  enableSnapshotForecast(); projectionResponse = () => json({ error: "internal_server_error" }, 500);
  await rendered(); fireEvent.click(screen.getByRole("button", { name: "Рассчитать" })); await settle();
  const mean = screen.getByTestId("setups-analytics-mean");
  const chart = screen.getByTestId("setups-daily-outcomes");
  const setup = screen.getByText("SYM1");
  const study = screen.getByRole("button", { name: "Подробности исследования 40" });
  expect(mean).toHaveTextContent("+1.25R"); expect(setup).toBeVisible();
  expect(chart.querySelector('circle[data-closed="2"]')).toBeInTheDocument();
  expect(analyticsCalls()).toHaveLength(1); expect(projectionCalls()).toHaveLength(0);
  await act(async () => { await vi.advanceTimersByTimeAsync(250); });
  const messages = screen.getAllByText(/^Модель недоступна:/);
  expect(messages).toHaveLength(1);
  expect(messages[0]).toHaveAttribute("role", "status");
  expect(messages[0]).toHaveTextContent(/^Модель недоступна: HTTP 500$/);
  expect(screen.getByTestId("setups-analytics-mean")).toBe(mean); expect(mean).toHaveTextContent("+1.25R");
  expect(screen.getByTestId("setups-daily-outcomes")).toBe(chart);
  expect(chart.querySelector('circle[data-closed="2"]')).toBeInTheDocument();
  expect(screen.getByText("SYM1")).toBe(setup); expect(setup).toBeVisible(); expect(study).toBeVisible();
  expect(screen.getByTestId("setups-model-summary")).toHaveTextContent(/^—$/);
  expect(screen.queryByTestId("setups-model-line")).not.toBeInTheDocument();
  const count = requests.length;
  await act(async () => { await vi.advanceTimersByTimeAsync(60000); });
  expect(requests).toHaveLength(count); expect(analyticsCalls()).toHaveLength(1); expect(projectionCalls()).toHaveLength(1);
  const writes = vi.mocked(fetch).mock.calls.filter(([, init]) => (init?.method ?? "GET") !== "GET");
  expect(writes.map(([path]) => {
    if (typeof path !== "string") throw new Error("Expected a string request URL");
    return new URL(path, "http://fixture").pathname;
  })).toEqual([
    "/v1/setups/analytics", "/v1/setups/analytics/forecast-view",
  ]);
  expect(requests.some((path) => /outcomes\/refresh|study-actual|replay|maintenance|optimization-studies/.test(path))).toBe(false);
});

it("draws every local calendar day independently of sparse labels and adds the finite fractional daily observed maximum, not the largest individual trade", async () => {
  storeRange("2026-10-01T00:00", "2026-11-01T00:00");
  analyticsResponse = () => json({ ...analyticsResult, setups: [rowAt(1, "2026-10-10T01:00:00Z", "tp", 3), rowAt(2, "2026-10-10T05:00:00Z", "sl", -1), rowAt(3, "2026-10-20T01:00:00Z", "tp", 2.125)] });
  await rendered(); fireEvent.click(screen.getByRole("button", { name: "Рассчитать" })); await settle();
  const chart = screen.getByTestId("setups-daily-outcomes");
  expect(chart.querySelectorAll("[data-grid-day] line")).toHaveLength(31);
  expect(chart.querySelectorAll("[data-grid-day] text").length).toBeLessThan(31);
  expect(chart.querySelector('[data-observed-max-r="2.125"]')).not.toBeNull();
  expect(chart.querySelector('[data-observed-max-r="3"]')).toBeNull();
  expect(within(chart).getByLabelText("Фактический максимум: 2.125 R")).toHaveTextContent("+2.13R");
  expect(chart.querySelector('circle[data-closed="2"]')).toHaveAttribute("data-mean-r", "1.00");
});

it("uses only compact saved metadata for best score, pure forecast and original recommended raw-R pair; factual denominator remains current selected cohort", async () => {
  metadataResponse = () => json({ studies: [{ ...studyMeta1, status: "completed", bestScore: 0.99,
    forecastBest: { forecastR: 1.9, meanR: 0.1, tpPct: 30, slPct: 2 }, bestCell: { tpPct: 14, slPct: 4, rawR: -0.3 } }, studyMeta2] });
  await rendered(); fireEvent.click(screen.getByRole("button", { name: "Рассчитать" })); await settle();
  const row = within(screen.getByRole("button", { name: "Подробности исследования 40" }).closest("tr")!);
  expect(row.getByText("+0.99R")).toBeInTheDocument(); expect(row.getByText("+1.90R")).toBeInTheDocument();
  expect(row.getByText("TP 14% / SL 4% · -0.30R")).toBeInTheDocument();
  expect(row.getByLabelText("Факт: 2 закрытых из 5 выбранных")).toHaveTextContent("2/5");
  expect(row.getByText("Завершено")).toHaveAttribute("data-slot", "badge");
  expect(requests.some((path) => /optimization-studies|study-actual|study-expectancy/.test(path))).toBe(false);
});

it("shows settings load/save failures and disables failed-load controls instead of silently applying defaults", async () => {
  settingsResponse = () => json({ error: "offline" }, 503);
  await rendered();
  expect(screen.getByRole("alert")).toHaveTextContent("Не удалось загрузить общие настройки");
  expect(screen.getByRole("button", { name: "Прогноз" })).toBeDisabled();
  settingsResponse = (init) => init?.method === "PUT" ? json({ error: "offline" }, 503) : json({ settings: { trendMode: "mean" } });
  fireEvent.click(screen.getByRole("button", { name: "Повторить загрузку настроек" })); await settle();
  fireEvent.click(screen.getByRole("button", { name: "Прогноз" }));
  await act(async () => { await vi.advanceTimersByTimeAsync(800); });
  expect(screen.getByRole("alert")).toHaveTextContent("Ошибка сохранения настроек");
  expect(screen.getByRole("button", { name: "Прогноз" })).toHaveAttribute("aria-pressed", "true");
});

it("keeps an inactive mounted Analytics owner synchronized and prevents its later unrelated autosave from overwriting newer Setups controls", async () => {
  settingsResponse = () => json({ settings: { trendMode: "mean", trendBendDays: "linear", clusterGapInput: "8",
    forecast: { method: "trend", version: 1, bendDays: "linear" }, sections: { "long-matrix": { unrelated: "keep" } } } });
  const savedFetch = globalThis.fetch;
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    if (path === "/v1/analytics/sets") return Promise.resolve(json({ sets: [{ setId: "run-12", label: "Набор 12", params: {}, setupCount: 0, fromUnix: 1700000000, toUnix: 1700600000 }] }));
    if (path.startsWith("/v1/analytics/months")) return Promise.resolve(json({ months: [] }));
    if (path === "/v1/analytics/cache/summary") return Promise.resolve(json({ tables: [], total: 0 }));
    return savedFetch(path, init);
  }));
  render(<><RouteActiveContext.Provider value={false}><div data-testid="inactive-analytics"><AnalyticsPage /></div></RouteActiveContext.Provider><SetupsAnalyticsPanel /></>);
  await settle();
  const analytics = within(screen.getByTestId("inactive-analytics"));
  const setups = within(screen.getByRole("region", { name: "Общий прогноз Analytics и Сетапов" }));
  fireEvent.change(analytics.getByLabelText("Разрыв кластера, ч"), { target: { value: "20" } });
  fireEvent.blur(analytics.getByLabelText("Разрыв кластера, ч"));
  fireEvent.click(setups.getByRole("button", { name: "Прогноз" }));
  fireEvent.change(setups.getByLabelText("Метод прогноза"), { target: { value: "decayed_probability" } });
  fireEvent.change(setups.getByLabelText("Память модели H, дней"), { target: { value: "7" } });
  expect(analytics.getByRole("button", { name: "Прогноз" })).toHaveAttribute("aria-pressed", "true");
  expect(analytics.getByLabelText("Память модели H, дней")).toHaveValue(7);
  await act(async () => { await vi.advanceTimersByTimeAsync(801); });
  const writes = vi.mocked(fetch).mock.calls.filter(([path, init]) => path === "/v1/settings/web-analytics" && init?.method === "PUT");
  expect(writes).toHaveLength(1);
  expect(JSON.parse(writes[0]![1]!.body as string)).toMatchObject({ clusterGapInput: "20", trendMode: "forecast",
    forecast: { method: "decayed_probability", halfLifeDays: "7" }, sections: { "long-matrix": { unrelated: "keep" } } });
  expect(analyticsCalls()).toHaveLength(0); expect(projectionCalls()).toHaveLength(0);
});

it("bounds the calendar SVG rather than allocating a day node for every day of arbitrary multi-decade input", async () => {
  storeRange("2000-01-01T00:00", "2026-11-13T12:00");
  await rendered(); fireEvent.click(screen.getByRole("button", { name: "Рассчитать" })); await settle();
  expect(screen.getByText(/диапазон календарной сетки превышает десять лет/)).toBeInTheDocument();
  expect(screen.queryByTestId("setups-daily-outcomes")).toBeNull();
  expect(screen.getByTestId("setups-analytics-mean")).toHaveTextContent("+1.25R");
});
