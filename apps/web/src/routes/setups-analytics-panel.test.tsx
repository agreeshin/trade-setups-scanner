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
      analytics: { clusterEnabled: true, clusterGapMinutes: 480, timeShiftMinutes: 5, priceChangePct: 2 },
      matrix: [{ tpPct: 20, slPct: 6, riskExpectancy: 0.9, avgMovePct: 1, closedCount: 3, tpCount: 2, slCount: 1, unclosedCount: 0, noDataCount: 0, priceChangeFilteredCount: 0, meanRiskExpectancy: 0.4, forecastRiskExpectancy: 0.44 }],
      support: null,
      provenance: { optimizerVersion: "v", seed: 1, snapshotHash: "h", capturedAt: "2026-11-01T00:00:00Z" },
      conclusions: [],
    },
    reason: "trial_budget",
  },
  error: null,
  forecastBest: { forecastR: 0.44, meanR: 0.3, tpPct: 20, slPct: 6 },
  createdAt: "2026-11-01T00:00:00Z", updatedAt: "2026-11-02T00:00:00Z", events: [],
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
    if (path === "/v1/algorithms") return Promise.resolve(json({ algorithms: [{ id: "price_volume_spike", name: "Рост цены и объёма", version: "1", description: "", enabled: true, params: {}, fields: [] }] }));
    if (path.startsWith("/v1/setups/analytics/studies")) return Promise.resolve(json({ studies: [studyMeta1, studyMeta2] }));
    if (path === "/v1/setups/analytics/study-expectancy/40") return Promise.resolve(json({ studyId: 40, observationCutoff: 1793923200, meanR: 0.2, daily: [{ dayUnix: 1793923200, meanR: 0.3, tpCount: 2, slCount: 1 }, { dayUnix: 1794009600, meanR: -0.1, tpCount: 1, slCount: 1 }], trend: { method: "mean", points: [{ dayUnix: 1793923200, value: 0.2 }, { dayUnix: 1794009600, value: 0.2 }] } }));
    if (path === "/v1/optimization-studies/40") return Promise.resolve(json(optimizationStudy40));
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
  expect(screen.getByRole("heading", { name: "Сетапы" })).toBeInTheDocument();
  expect(screen.getByText("SYM1")).toBeInTheDocument();
  expect(screen.getAllByText("TP")).toHaveLength(2);
  expect(screen.getAllByText("SL")).toHaveLength(2);
  expect(screen.getByText("не закрылась")).toBeInTheDocument();
  expect(screen.getByText("нет данных")).toBeInTheDocument();
  expect(screen.getByText("гейт не пройден")).toBeInTheDocument();
  expect(screen.getAllByText("№1").length).toBeGreaterThan(0);
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
  expect(within(dialog).getByText("spike_mult")).toBeInTheDocument();
  expect(within(dialog).getAllByText("2.5").length).toBeGreaterThan(0);
  const chart = within(dialog).getByTestId("study-horizon-chart");
  expect(chart.querySelector("[data-divider-day]")).not.toBeNull();
  expect(chart.querySelectorAll('[data-phase="past"]').length).toBe(2);
  expect(dialog.querySelector("img")).toBeNull();
  expect(requests).toContain("/v1/optimization-studies/40");
  expect(requests.some((path) => path === "/v1/setups/analytics/study-expectancy/40")).toBe(true);
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
