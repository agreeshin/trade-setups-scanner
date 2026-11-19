import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { SetupsAnalyticsPanel } from "./setups-analytics-panel";

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
let analyticsResponse: () => Response = () => json(analyticsResult);

beforeEach(() => {
  vi.useFakeTimers(); vi.setSystemTime(new Date("2026-11-13T12:00:00Z")); requests.length = 0;
  analyticsResponse = () => json(analyticsResult);
  window.localStorage.clear();
  Object.defineProperty(URL, "createObjectURL", { configurable: true, writable: true, value: vi.fn(() => "blob:expectancy-png") });
  Object.defineProperty(URL, "revokeObjectURL", { configurable: true, writable: true, value: vi.fn() });
  vi.stubGlobal("fetch", vi.fn((path: string) => {
    requests.push(path);
    if (path === "/v1/algorithms") return Promise.resolve(json({ algorithms: [{ id: "price_volume_spike", name: "Рост цены и объёма", version: "1", description: "", enabled: true, params: {}, fields: [{ key: "spike_mult", label: "Множитель объёма", type: "decimal", min: 1, max: 20, unit: "×" }, { key: "min_quote_vol", label: "Мин. объём котировки", type: "decimal", min: 0, max: 1000000000, unit: "USDT" }] }] }));
    if (path.startsWith("/v1/setups/analytics/studies")) return Promise.resolve(json({ studies: [studyMeta1, studyMeta2] }));
    if (path === "/v1/setups/analytics/study-expectancy/40") {
      const horizonDays = Array.from({ length: 13 }, (_, index) => 1793923200 + index * 86400);
      return Promise.resolve(json({ studyId: 40, observationCutoff: 1794009600, meanR: 0.2, daily: horizonDays.map((dayUnix, index) => ({ dayUnix, meanR: index % 2 === 0 ? 0.3 : -0.1, tpCount: 1, slCount: 1 })), trend: { method: "mean", points: [{ dayUnix: horizonDays[0]!, value: 0.2 }, { dayUnix: horizonDays.at(-1)!, value: 0.2 }] } }));
    }
    if (path === "/v1/optimization-studies/40") return Promise.resolve(json(optimizationStudy40));
    if (path === "/v1/optimization-studies/35") return Promise.resolve(json(optimizationStudy35));
    if (path === "/v1/setups/analytics/study-expectancy/35") {
      const horizonDays = Array.from({ length: 13 }, (_, index) => 1793923200 + index * 86400);
      return Promise.resolve(json({ studyId: 35, observationCutoff: 1794009600, meanR: 0.2, daily: horizonDays.map((dayUnix, index) => ({ dayUnix, meanR: index % 2 === 0 ? 0.3 : -0.1, tpCount: 1, slCount: 1 })), trend: { method: "mean", points: [{ dayUnix: horizonDays[0]!, value: 0.2 }, { dayUnix: horizonDays.at(-1)!, value: 0.2 }] } }));
    }
    if (path.startsWith("/v1/setups/analytics?")) return Promise.resolve(analyticsResponse());
    return Promise.resolve(json({}));
  }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

const settle = async () => { await act(async () => { await vi.advanceTimersByTimeAsync(0); }); };
const analyticsCalls = () => requests.filter((path) => path.startsWith("/v1/setups/analytics?"));
const studiesCalls = () => requests.filter((path) => path.startsWith("/v1/setups/analytics/studies"));

async function rendered() {
  render(<SetupsAnalyticsPanel />);
  await settle();
  await settle();
  return null;
}

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
  expect(screen.getByText("не закрылась")).toBeInTheDocument();
  expect(screen.getByText("нет данных")).toBeInTheDocument();
  expect(screen.getByText("4/5")).toBeInTheDocument();
  expect(screen.queryByText("гейт не пройден")).toBeNull();
  expect(screen.queryByText("SYM5")).toBeNull();
  expect(screen.getAllByText("№40").length).toBe(4);
  fireEvent.click(screen.getByLabelText("Скрыть сетапы, которые не прошли гейт"));
  expect(screen.getByText("гейт не пройден")).toBeInTheDocument();
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
