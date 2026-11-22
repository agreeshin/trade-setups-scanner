import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { summarizeOptimizationStudy, type OptimizationSpec, type OptimizationStudy } from "../lib/optimization-api";
import { notCalculatedActualResponse } from "../../test/study-actual-fixture";
import { applyInterfaceSettings, formatUnix, interfaceSettings } from "../lib/datetime-format";
import { AlgorithmsPage } from "./algorithms-page";
import type { Algorithm } from "./platform-controls";

const algorithm: Algorithm = {
  id: "future_algorithm", name: "Новый алгоритм", version: "2.0", description: "Metadata driven", enabled: true,
  params: { futureThreshold: "2.5", futureWindow: 3 },
  fields: [
    { key: "futureThreshold", label: "Новый порог", type: "decimal", min: 0.1, max: 100, unit: "%", optimization: { enabledByDefault: true, values: ["1.5", "2.5", "4"] } },
    { key: "futureWindow", label: "Новое окно", type: "integer", min: 1, max: 50, unit: "мин" },
  ],
};
const sharedParams = { windowMinutes: 5, volumeMultiplier: "3", baselineLongDays: 30, baselineShortDays: 7, cooldownMinutes: 5, maxGapPercent: "1", maxConsecutiveGapMinutes: 5, requiredRecentMinutes: 10 };
const sharedFields: Algorithm["fields"] = [
  { key: "windowMinutes", label: "Окно", type: "integer", min: 1, max: 10080, unit: "мин", optimization: { enabledByDefault: true, values: [3, 5] } },
  { key: "volumeMultiplier", label: "Множитель объёма", type: "decimal", min: 0.1, max: 100, unit: "×", optimization: { enabledByDefault: true, values: ["2", "3"] } },
  { key: "baselineLongDays", label: "Длинная база", type: "integer", min: 1, max: 365, unit: "дни" },
  { key: "baselineShortDays", label: "Короткая база", type: "integer", min: 1, max: 365, unit: "дни" },
  { key: "cooldownMinutes", label: "Отсечка повторов", type: "integer", min: 0, max: 10080, unit: "мин" },
  { key: "maxGapPercent", label: "Допуск пропусков", type: "decimal", min: 0, max: 100, unit: "%" },
  { key: "maxConsecutiveGapMinutes", label: "Пропусков подряд", type: "integer", min: 0, max: 10080, unit: "мин" },
  { key: "requiredRecentMinutes", label: "Последних минут без пропусков", type: "integer", min: 0, max: 10080, unit: "мин" },
];
const metadataAlgorithms: Algorithm[] = [
  { id: "price_volume_spike", name: "Рост цены и объёма", version: "1.1.1", description: "Рост", enabled: true,
    params: { priceGrowthPct: "12", ...sharedParams }, fields: [{ key: "priceGrowthPct", label: "Рост цены", type: "decimal", min: 0, max: 100, unit: "%", optimization: { enabledByDefault: true, values: ["6", "12"] } }, ...sharedFields] },
  { id: "price_drop_volume_spike", name: "Падение цены и всплеск оборота", version: "1.1.1", description: "Падение", enabled: true,
    params: { priceDropPct: "12", ...sharedParams, metadataOnlyWindow: 7 }, fields: [...sharedFields].reverse().concat([
      { key: "priceDropPct", label: "Падение цены", type: "decimal", min: 0, max: 100, unit: "%", optimization: { enabledByDefault: true, values: ["6", "12"] } },
      { key: "metadataOnlyWindow", label: "Дополнительное окно", type: "integer", min: 5, max: 20, unit: "мин" },
    ]) },
];
const instruments = [
  { id: 1, exchange: "bybit", category: "linear", symbol: "BTCUSDT", status: "Trading" },
  { id: 2, exchange: "bybit", category: "linear", symbol: "OLDUSDT", status: "Delisted" },
];
const spec: OptimizationSpec = {
  algorithmId: algorithm.id, algorithmVersion: algorithm.version, algorithmParams: algorithm.params,
  algorithmSearch: { futureThreshold: ["1.5", "2.5", "4"] },
  analyticsParams: { clusterEnabled: true, clusterGapMinutes: 480, timeShiftMinutes: 0, priceChangePct: null }, analyticsSearch: {},
  instruments: instruments.map(({ exchange, category, symbol }) => ({ exchange, category, symbol })),
  fromUnix: 1_793_491_200, toUnix: 1_794_096_000, direction: "short", setupMaturity: 0, horizonMinutes: 10080,
  tpLevels: [4, 6], slLevels: [4, 6], minClosedPerCell: 1, seed: 42, maxTrials: 256, maxSeconds: 3600,
};
const progress = { phase: "acquiring", evaluatedCandidates: 0, scorableCandidates: 0, maxTrials: 256, elapsedSeconds: 12, bestScore: null as number | null };
function record(status: OptimizationStudy["status"] = "running"): OptimizationStudy {
  return { id: 17, status, spec, progress, result: null as OptimizationStudy["result"], error: null, createdAt: "2026-11-10T00:00:00Z", updatedAt: "2026-11-10T00:01:00Z", events: [{ at: "2026-11-10T00:00:00Z", kind: "started", message: "Подготовка снимка" }] };
}
function winnerRecord(mode: "mean" | "forecast" | "blend" = "blend", partial = false): OptimizationStudy {
  const saved = record(partial ? "cancelled" : "completed");
  const forecast = { method: "trend", version: 1, bendDays: "30" } as const;
  const winnerProgress = { ...progress, phase: "done", bestScore: 0.23145202240242907 };
  return { ...saved, progress: winnerProgress, spec: { ...spec, trendMode: mode, trendBlendLambda: 0.6, trendBendDays: "30", forecast },
    forecastBest: { forecastR: 0.2695393706707151, meanR: 0.174321, tpPct: 4, slPct: 6 },
    result: { progress: winnerProgress, reason: partial ? "cancelled" : "time_budget", best: {
      score: 0.23145202240242907, meanScore: 0.174321, trendMode: mode, trendBlendLambda: 0.6, trendBendDays: "30", forecast, partial,
      params: algorithm.params, analytics: spec.analyticsParams,
      matrix: [{ tpPct: 4, slPct: 6, riskExpectancy: 0.23145202240242907, meanRiskExpectancy: 0.174321, forecastRiskExpectancy: 0.2695393706707151,
        avgMovePct: -0.123456, closedCount: 100000, tpCount: 65000, slCount: 35000, unclosedCount: 2, noDataCount: 1, priceChangeFilteredCount: 0 }],
      provenance: { optimizerVersion: "fixture", seed: 42, snapshotHash: "modal-layout", capturedAt: saved.createdAt }, conclusions: [],
    } } };
}
let studies: ReturnType<typeof record>[];
let createStatus: number;
let deleteStatus: number;
let deleteErrorCode: string;
let cancelStatus: number;
let listedAlgorithms: Algorithm[];
let draftStatus: number;
let drafts: Record<string, unknown>;
const cleanupFixture = { eligible: true, blockers: [] as string[], fingerprint: "a".repeat(32),
  counts: { studies: 12, generations: 7, profiles: 3, checkpoints: 2, notificationJobs: 1, processesReset: 2 },
  retained: { runs: 9, batches: 4, qualityIntervals: 20, analyticsResults: 4, liveSetups: 19, configuredProcesses: 2, configuredSlots: 6 } };
let cleanupPreview = cleanupFixture;
let cleanupStatus = 200;
const requests: { path: string; method: string; body: unknown; signal?: AbortSignal | null }[] = [];
const initialInterfaceSettings = interfaceSettings();
beforeEach(() => {
  studies = []; createStatus = 202; deleteStatus = 200; deleteErrorCode = "delete_failed"; cancelStatus = 202; draftStatus = 200; drafts = {}; requests.length = 0;
  listedAlgorithms = [algorithm, { ...algorithm, id: "second_algorithm", name: "Второй алгоритм" }];
  cleanupPreview = cleanupFixture; cleanupStatus = 200;
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    const body: unknown = typeof init?.body === "string" ? JSON.parse(init.body) : null;
    requests.push({ path, method, body, ...(init?.signal ? { signal: init.signal } : {}) });
    const json = (value: unknown, status = 200) => Promise.resolve(new Response(JSON.stringify(value), { status }));
    if (path === "/v1/algorithms") return json({ algorithms: listedAlgorithms });
    if (path === "/v1/instruments") return json({ instruments });
    if (path === "/v1/continuous-studies/cleanup") {
      if (method === "GET") return json(cleanupPreview);
      if (cleanupStatus !== 200) return json({ error: cleanupStatus === 409 ? "continuous_cleanup_preview_stale" : "cleanup_failed", preview: cleanupPreview }, cleanupStatus);
      studies = studies.filter((study) => study.origin !== "continuous");
      return json({ deleted: cleanupPreview.counts, retained: cleanupPreview.retained });
    }
    if (path === "/v1/settings/ui") return json({ value: { replay: { symbols: "BTCUSDT\nOLDUSDT" } } });
    const draft = /^\/v1\/optimization-drafts\/(.+)$/.exec(path);
    if (draft) {
      const id = decodeURIComponent(draft[1]!);
      if (method === "PUT") {
        if (draftStatus !== 200) return json({ error: "draft_failed" }, draftStatus);
        drafts[id] = (body as { value: unknown }).value;
      }
      return json({ algorithmId: id, value: drafts[id] ?? null });
    }
    if (path === "/v1/optimization-studies" && method === "POST") {
      if (createStatus === 409) { studies = [record()]; return json({ error: "optimization_busy", activeStudyId: 17 }, 409); }
      const created = { ...record("queued"), spec: body as typeof spec };
      studies = [created]; return json(created, 202);
    }
    if (path.startsWith("/v1/optimization-studies?")) {
      const query = new URL(path, "http://localhost").searchParams;
      const id = query.get("algorithmId");
      const limit = Number(query.get("limit")); const offset = Number(query.get("offset"));
      const ordered = studies.filter((study) => !id || study.spec.algorithmId === id).sort((a, b) => Number(["queued", "running", "cancel_requested"].includes(b.status)) - Number(["queued", "running", "cancel_requested"].includes(a.status)) || b.id - a.id);
      return json({ studies: ordered.slice(offset, offset + limit).map(summarizeOptimizationStudy), total: ordered.length, limit, offset });
    }
    const cancel = /^\/v1\/optimization-studies\/(\d+)\/cancel$/.exec(path);
    if (cancel) {
      if (cancelStatus !== 202) return json({ error: "cancel_failed" }, cancelStatus);
      studies = studies.map((study) => study.id === Number(cancel[1]) ? { ...study, status: "cancel_requested" } : study); return json(studies.find((study) => study.id === Number(cancel[1])), 202);
    }
    const actual = /^\/v1\/optimization-studies\/(\d+)\/actual(?:\?.*)?$/.exec(path);
    if (actual) return json({ ...notCalculatedActualResponse(Number(actual[1])), originalReport: null });
    const detail = /^\/v1\/optimization-studies\/(\d+)$/.exec(path);
    if (detail) {
      const id = Number(detail[1]);
      if (method === "DELETE") {
        if (deleteStatus !== 200) return json({ error: deleteErrorCode }, deleteStatus);
        studies = studies.filter((study) => study.id !== id); return json({ deleted: true, id });
      }
      const study = studies.find((study) => study.id === id);
      return study ? json(study) : json({ error: "not_found" }, 404);
    }
    return json({});
  }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); applyInterfaceSettings(initialInterfaceSettings); });

// Discovery and paged history are separate reads. The larger inline table can
// exhaust the library's 1s query default before either read finishes in a full run.
function historyButton(name: string, queries: Pick<typeof screen, "findByRole"> = screen) {
  return queries.findByRole("button", { name }, { timeout: 5000 });
}

async function openOptimizer() {
  const flexible = await screen.findByRole("tab", { name: "Live-сканирование с гибкими параметрами" });
  if (flexible.getAttribute("aria-selected") !== "true") fireEvent.click(flexible);
  await userEvent.click(await screen.findByRole("button", { name: "Запустить новое исследование Новый алгоритм" }));
  return screen.findByRole("dialog", { name: "Оптимизация · Новый алгоритм" });
}
function enterDates() {
  fireEvent.change(screen.getByLabelText("Начало периода сигналов"), { target: { value: "2026-10-01T00:00" } });
  fireEvent.change(screen.getByLabelText("Конец периода сигналов"), { target: { value: "2026-10-03T00:00" } });
}

it("shows retained Segmented live panels and keeps fixed controls independent when continuous is unavailable", async () => {
  render(<AlgorithmsPage />);
  const first = within((await screen.findByText("Новый алгоритм · 2.0")).closest("form")!);
  expect(first.getAllByRole("tab").map((tab) => tab.getAttribute("aria-label"))).toEqual([
    "Live-сканирование с гибкими параметрами", "Live-сканирование с постоянными параметрами",
  ]);
  expect(first.getAllByRole("separator")).toHaveLength(3);
  for (const divider of first.getAllByRole("separator")) expect(divider).toHaveClass("my-4");
  const flexible = within(first.getByRole("region", { name: "Live-сканирование с гибкими параметрами" }));
  expect(flexible.getByRole("switch", { name: "Разрешить адаптивное live-сканирование" })).toBeDisabled();
  expect(flexible.getByRole("button", { name: "Сохранить разрешение адаптивного live" })).toBeDisabled();
  expect(first.getByRole("button", { name: "Запустить новое исследование Новый алгоритм" })).toHaveTextContent("Запустить новое исследование");
  expect(first.queryByRole("button", { name: "Посмотреть весь список" })).not.toBeInTheDocument();
  expect(first.getByRole("heading", { name: "Исследования оптимальных параметров" })).toBeInTheDocument();
  expect(screen.queryByText("Статус / результат оптимизации")).not.toBeInTheDocument();
  const history = first.getByRole("region", { name: "История оптимизации · Новый алгоритм" });
  expect(within(history).queryByRole("heading", { name: "Последние исследования" })).not.toBeInTheDocument();
  expect(within(history).getByRole("button", { name: "Запустить новое исследование Новый алгоритм" }).closest("div")?.nextElementSibling).toContainElement(within(history).getByRole("table"));
  await userEvent.click(first.getByRole("tab", { name: "Live-сканирование с постоянными параметрами" }));
  const constant = within(first.getByRole("tabpanel", { name: "Live-сканирование с постоянными параметрами" }));
  const toggle = constant.getByRole("checkbox", { name: "Включён для live-сканирования" });
  expect(toggle).toBeChecked();
  await userEvent.click(toggle);
  expect(toggle).not.toBeChecked();
  expect(requests.every((request) => request.method === "GET")).toBe(true);
  await userEvent.click(constant.getByRole("button", { name: "Сохранить алгоритм" }));
  await constant.findByText("Настройки алгоритма сохранены.");
  expect(requests.filter((request) => request.method !== "GET")).toEqual([
    expect.objectContaining({ path: `/v1/algorithms/${algorithm.id}`, method: "PUT", body: { params: algorithm.params, enabled: false } }),
  ]);
  await userEvent.click(screen.getByRole("tab", { name: "Второй алгоритм" }));
  await userEvent.click(screen.getByRole("tab", { name: "Live-сканирование с постоянными параметрами" }));
  expect(screen.getByRole("checkbox", { name: "Включён для live-сканирования" })).toBeChecked();
  await userEvent.click(screen.getByRole("tab", { name: "Новый алгоритм" }));
  expect(toggle).not.toBeChecked();
});

it("shows one retained algorithm panel and derives live dots from saved enabled flags only", async () => {
  listedAlgorithms[1] = { ...listedAlgorithms[1]!, enabled: false };
  const { container } = render(<AlgorithmsPage />);
  const firstTab = await screen.findByRole("tab", { name: "Новый алгоритм" });
  const secondTab = screen.getByRole("tab", { name: "Второй алгоритм" });
  expect(screen.getByRole("heading", { name: "Алгоритмы" })).toHaveClass("sr-only");
  expect(screen.getAllByRole("tabpanel")).toHaveLength(2);
  expect(firstTab).toHaveAccessibleDescription("Live-сканирование включено");
  expect(secondTab.querySelector('[data-slot="sidebar-status-dot"]')).toBeNull();
  await userEvent.click(screen.getByRole("tab", { name: "Live-сканирование с постоянными параметрами" }));
  const firstInput = screen.getByRole("spinbutton", { name: "Новый порог (%)" });
  fireEvent.change(firstInput, { target: { value: "8.5" } });
  await userEvent.click(screen.getByRole("checkbox", { name: "Включён для live-сканирования" }));
  expect(firstTab.querySelector('[data-slot="sidebar-status-dot"]')).toBeInTheDocument();
  await userEvent.click(secondTab);
  await userEvent.click(screen.getByRole("tab", { name: "Live-сканирование с постоянными параметрами" }));
  expect(firstInput).toBeInTheDocument(); expect(firstInput).not.toBeVisible();
  const secondInput = screen.getByRole("spinbutton", { name: "Новый порог (%)" });
  expect(secondInput).not.toBe(firstInput); expect(secondInput).toHaveValue(2.5);
  fireEvent.change(secondInput, { target: { value: "9.5" } });
  await userEvent.click(firstTab);
  expect(screen.getByRole("spinbutton", { name: "Новый порог (%)" })).toBe(firstInput);
  expect(firstInput).toHaveValue(8.5);
  expect(screen.getByRole("checkbox", { name: "Включён для live-сканирования" })).not.toBeChecked();
  expect(screen.getAllByRole("tabpanel")).toHaveLength(2);
  expect(container.querySelectorAll('[data-slot="page-sidebar-content"] form')).toHaveLength(2);
  await userEvent.click(screen.getByRole("button", { name: "Сохранить алгоритм" }));
  await screen.findByText("Настройки алгоритма сохранены.");
  expect(firstTab).not.toHaveAttribute("aria-describedby");
  expect(firstTab.querySelector('[data-slot="sidebar-status-dot"]')).toBeNull();
  await userEvent.click(secondTab);
  expect(secondInput).toHaveValue(9.5);
  await userEvent.click(screen.getByRole("checkbox", { name: "Включён для live-сканирования" }));
  expect(secondTab.querySelector('[data-slot="sidebar-status-dot"]')).toBeNull();
  await userEvent.click(screen.getByRole("button", { name: "Сохранить алгоритм" }));
  await waitFor(() => expect(secondTab).toHaveAccessibleDescription("Live-сканирование включено"));
  expect(secondTab.querySelector('[data-slot="sidebar-status-dot"]')).toHaveClass("bg-success");
  expect(requests.filter((request) => request.path === "/v1/algorithms")).toHaveLength(1);
  expect(requests.filter((request) => request.method !== "GET").map((request) => request.method)).toEqual(["PUT", "PUT"]);
});

it("builds unknown fields from metadata and submits the complete typed scope only explicitly", async () => {
  render(<AlgorithmsPage />);
  const dialog = await openOptimizer();
  // This is a payload contract: label queries avoid repeatedly computing accessible
  // names/styles across the full form. Pointer/focus behavior has separate tests.
  const controls = within(dialog);
  expect(await controls.findByLabelText("Значения: Новый порог")).toHaveValue("1.5, 2.5, 4");
  expect(controls.getByLabelText("Подбирать: Новый порог")).toBeChecked();
  expect(controls.getByLabelText("Подбирать: Новое окно")).not.toBeChecked();
  expect(controls.getByLabelText("OLDUSDT (Delisted)")).toBeChecked();
  const expectation = within(controls.getByRole("region", { name: "Оптимизируемое матожидание" }));
  expect(expectation.getByLabelText("Минимальный прогноз матрицы, R")).toHaveValue(0);
  expect(controls.queryByLabelText("Минимальное среднее матожидание, R")).not.toBeInTheDocument();
  const frequency = controls.getByLabelText("Минимум сетапов в день");
  expect(frequency).toHaveValue(4);
  expect(frequency).toHaveAttribute("type", "number");
  expect(frequency).toHaveAttribute("min", "0");
  expect(frequency).toHaveAttribute("step", "0.1");
  expect(frequency).toHaveAccessibleDescription(/точн.*период.*0 отключает/);
  expect(requests.every((request) => request.method === "GET")).toBe(true);
  fireEvent.change(controls.getByLabelText("Начало периода сигналов"), { target: { value: "2026-10-01T00:00" } });
  fireEvent.change(controls.getByLabelText("Конец периода сигналов"), { target: { value: "2026-10-03T00:00" } });
  fireEvent.change(controls.getByLabelText("Направление сделки"), { target: { value: "long" } });
  fireEvent.change(controls.getByLabelText("Уровни TP, %"), { target: { value: "4, 25" } });
  fireEvent.change(controls.getByLabelText("Уровни SL, %"), { target: { value: "0.5, 6" } });
  fireEvent.click(controls.getByLabelText("Подбирать: Новое окно"));
  fireEvent.change(controls.getByLabelText("Значения: Новое окно"), { target: { value: "2, 3, 5" } });
  expect(controls.getByLabelText("Подбирать: Кластеризация")).toBeChecked();
  expect(controls.getByLabelText("Подбирать: Гейт цены, %")).toBeChecked();
  fireEvent.change(controls.getByLabelText("Значения: Гейт цены, %"), { target: { value: "null, 0, 1" } });
  const start = controls.getByRole("button", { name: "Начать исследование" });
  await waitFor(() => expect(start).toBeEnabled());
  fireEvent.click(start); fireEvent.click(start);
  expect(await screen.findByText("Исследование #17")).toBeInTheDocument();
  const creates = requests.filter((request) => request.method === "POST");
  expect(creates).toHaveLength(1);
  expect(creates[0]?.body).not.toHaveProperty("minMeanExpectancy");
  expect(creates[0]?.body).toEqual({
    ...spec, fromUnix: new Date(2026, 9, 1).getTime() / 1000, toUnix: new Date(2026, 9, 3).getTime() / 1000,
    direction: "long", tpLevels: [4, 25], slLevels: [0.5, 6], minSetupsPerDay: 4,
    trendMode: "mean", trendBendDays: "linear", minForecastExpectancy: 0,
    forecast: { method: "decayed_probability", version: 1, halfLifeDays: 14, kappa: 10, priorPolicy: "gross_break_even" },
    algorithmSearch: { futureThreshold: ["1.5", "2.5", "4"], futureWindow: [2, 3, 5] },
    analyticsSearch: { clusterEnabled: [false, true], clusterGapMinutes: [240, 480], timeShiftMinutes: [0, 5], priceChangePct: [null, 0, 1] },
  });
  expect(requests.some((request) => request.method === "PUT" && !request.path.startsWith("/v1/optimization-drafts/"))).toBe(false);
});

it.each([0, 1])("switches algorithm cards without stale metadata and submits only algorithm %s keys", async (targetIndex) => {
  listedAlgorithms = metadataAlgorithms;
  const target = metadataAlgorithms[targetIndex]!;
  const other = metadataAlgorithms[1 - targetIndex]!;
  const ownKey = targetIndex === 0 ? "priceGrowthPct" : "priceDropPct";
  const otherKey = targetIndex === 0 ? "priceDropPct" : "priceGrowthPct";
  const ownLabel = targetIndex === 0 ? "Рост цены" : "Падение цены";
  const otherLabel = targetIndex === 0 ? "Падение цены" : "Рост цены";
  render(<AlgorithmsPage />);
  await userEvent.click(await screen.findByRole("tab", { name: other.name }));
  fireEvent.click(await screen.findByRole("button", { name: `Запустить новое исследование ${other.name}` }));
  const previous = within(await screen.findByRole("dialog", { name: `Оптимизация · ${other.name}` }));
  fireEvent.change(await previous.findByLabelText(`Baseline: ${otherLabel}`), { target: { value: "91" } });
  await userEvent.click(previous.getByRole("button", { name: "Закрыть" }));
  await userEvent.click(screen.getByRole("tab", { name: target.name }));
  fireEvent.click(screen.getByRole("button", { name: `Запустить новое исследование ${target.name}` }));
  const controls = within(await screen.findByRole("dialog", { name: `Оптимизация · ${target.name}` }));
  expect(await controls.findByLabelText(`Baseline: ${ownLabel}`)).toHaveValue("12");
  expect(controls.queryByLabelText(`Baseline: ${otherLabel}`)).not.toBeInTheDocument();
  expect(controls.queryByLabelText(`Подбирать: ${otherLabel}`)).not.toBeInTheDocument();
  expect(controls.getAllByLabelText(/^Baseline:/).slice(0, target.fields.length)).toEqual(target.fields.map((field) => controls.getByLabelText(`Baseline: ${field.label}`)));
  for (const field of target.fields) {
    expect(controls.getByLabelText(`Baseline: ${field.label}`)).toHaveValue(String(target.params[field.key]));
    if (field.optimization?.enabledByDefault) expect(controls.getByLabelText(`Подбирать: ${field.label}`)).toBeChecked();
    else expect(controls.getByLabelText(`Подбирать: ${field.label}`)).not.toBeChecked();
  }
  fireEvent.change(controls.getByLabelText(`Baseline: ${ownLabel}`), { target: { value: "13.25" } });
  fireEvent.change(controls.getByLabelText(`Значения: ${ownLabel}`), { target: { value: "13.25, 20" } });
  if (targetIndex === 1) {
    fireEvent.change(controls.getByLabelText("Baseline: Дополнительное окно"), { target: { value: "9" } });
    fireEvent.click(controls.getByLabelText("Подбирать: Дополнительное окно"));
    fireEvent.change(controls.getByLabelText("Значения: Дополнительное окно"), { target: { value: "7, 9" } });
  } else expect(controls.queryByLabelText("Baseline: Дополнительное окно")).not.toBeInTheDocument();
  enterDates();
  const start = controls.getByRole("button", { name: "Начать исследование" });
  await waitFor(() => expect(start).toBeEnabled()); fireEvent.click(start);
  await screen.findByText("Исследование #17");
  const payload = requests.find((request) => request.path === "/v1/optimization-studies" && request.method === "POST")?.body as OptimizationSpec;
  expect(payload.algorithmId).toBe(target.id);
  expect(payload.algorithmVersion).toBe(target.version);
  expect(payload.algorithmParams).toEqual({ ...target.params, [ownKey]: "13.25", ...(targetIndex === 1 ? { metadataOnlyWindow: 9 } : {}) });
  expect(payload.algorithmSearch).toEqual({ windowMinutes: [3, 5], volumeMultiplier: ["2", "3"], [ownKey]: ["13.25", "20"], ...(targetIndex === 1 ? { metadataOnlyWindow: [7, 9] } : {}) });
  expect(payload.algorithmParams).not.toHaveProperty(otherKey);
  expect(payload.algorithmSearch).not.toHaveProperty(otherKey);
  expect(requests.some((request) => request.method === "PUT" && !request.path.startsWith("/v1/optimization-drafts/"))).toBe(false);
});

it("submits the selected forecast metric and persists its bend in the shared draft", async () => {
  render(<AlgorithmsPage />);
  const dialog = within(await openOptimizer());
  await dialog.findByLabelText("Baseline: Новый порог");
  expect(dialog.getByRole("button", { name: "Среднее" })).toHaveAttribute("aria-pressed", "true");
  expect(dialog.queryByLabelText("Перегиб на N дней")).not.toBeInTheDocument();
  fireEvent.click(dialog.getByRole("button", { name: "Прогноз" }));
  expect(dialog.getByLabelText("Метод прогноза")).toHaveValue("decayed_probability");
  expect(dialog.getByLabelText("Память модели H, дней")).toHaveValue(14);
  fireEvent.change(dialog.getByLabelText("Метод прогноза"), { target: { value: "trend" } });
  const bend = dialog.getByLabelText("Перегиб на N дней");
  expect(bend).toHaveValue("linear"); expect(bend).toBeEnabled();
  fireEvent.change(bend, { target: { value: "14" } });
  fireEvent.blur(bend);
  await waitFor(() => expect(drafts.future_algorithm).toMatchObject({ shared: { scope: { trendMode: "forecast", trendBendDays: "14", forecast: { method: "trend", version: 1, bendDays: "14" } } } }));
  enterDates();
  const start = dialog.getByRole("button", { name: "Начать исследование" });
  await waitFor(() => expect(start).toBeEnabled()); fireEvent.click(start);
  await screen.findByText("Исследование #17");
  expect(requests.find((request) => request.path === "/v1/optimization-studies" && request.method === "POST")?.body)
    .toMatchObject({ trendMode: "forecast", trendBendDays: "14" });
});

it("edits, persists and submits a new probability recipe while keeping mean the initial mode", async () => {
  render(<AlgorithmsPage />); let dialog = within(await openOptimizer());
  await dialog.findByLabelText("Baseline: Новый порог");
  expect(dialog.getByRole("button", { name: "Среднее" })).toHaveAttribute("aria-pressed", "true");
  expect(dialog.queryByLabelText("Память модели H, дней")).not.toBeInTheDocument();
  fireEvent.click(dialog.getByRole("button", { name: "Прогноз" }));
  const halfLife = dialog.getByLabelText("Память модели H, дней");
  expect(halfLife).toHaveValue(14); expect(halfLife).not.toHaveAttribute("max");
  expect(dialog.queryByLabelText("Перегиб на N дней")).not.toBeInTheDocument();
  fireEvent.change(halfLife, { target: { value: "28" } });
  fireEvent.click(dialog.getByRole("button", { name: "Расширенные настройки модели" }));
  const kappa = dialog.getByLabelText("Сила prior κ");
  expect(kappa).toHaveValue(10); expect(kappa).not.toHaveAttribute("max");
  expect(dialog.getByLabelText("Политика prior")).toHaveValue("gross_break_even");
  expect(dialog.getByText(/не калиброванный оптимум/)).toBeInTheDocument();
  expect(dialog.queryByRole("checkbox", { name: /Подбирать:.*(H|κ|prior)/ })).not.toBeInTheDocument();
  fireEvent.change(kappa, { target: { value: "5" } }); fireEvent.blur(kappa);
  await waitFor(() => expect(drafts.future_algorithm).toMatchObject({ shared: { scope: { trendMode: "forecast", forecast: {
    method: "decayed_probability", version: 1, halfLifeDays: "28", kappa: "5", priorPolicy: "gross_break_even",
  } } } }));
  await userEvent.click(dialog.getByRole("button", { name: "Закрыть" }));
  dialog = within(await openOptimizer());
  expect(await dialog.findByLabelText("Память модели H, дней")).toHaveValue(28);
  fireEvent.click(dialog.getByRole("button", { name: "Расширенные настройки модели" }));
  expect(dialog.getByLabelText("Сила prior κ")).toHaveValue(5);
  enterDates(); fireEvent.click(dialog.getByRole("button", { name: "Начать исследование" }));
  await screen.findByText("Исследование #17");
  const submitted = requests.find((request) => request.path === "/v1/optimization-studies" && request.method === "POST")?.body as OptimizationSpec;
  expect(submitted.forecast).toEqual({ method: "decayed_probability", version: 1, halfLifeDays: 28, kappa: 5, priorPolicy: "gross_break_even" });
  expect(submitted.algorithmSearch).not.toHaveProperty("forecast");
  expect(submitted.analyticsSearch).not.toHaveProperty("forecast");
});

it.each([
  ["mean", false], ["forecast", false], ["blend", false],
  ["mean", true], ["forecast", true], ["blend", true],
] as const)("moves the complete %s score explanation (partial=%s) into a right-hand accessible InfoNote", async (mode, partial) => {
  studies = [winnerRecord(mode, partial)];
  render(<AlgorithmsPage />); await openOptimizer();
  const heading = await screen.findByRole("heading", { name: "Лучший score: 0.23 R" });
  const current = screen.getByText("Текущий лучший score").nextElementSibling!;
  expect(current).toHaveTextContent("0.23 R");
  const explanation = `${partial ? "Частичный итог остановленного исследования." : "Лучший полностью проверенный кандидат в пределах бюджета."} Это не доказанный глобальный оптимум. Score — равное среднее ${mode === "blend" ? "blend R" : mode === "forecast" ? "прогноза R" : "R"} всех выбранных TP×SL-ячеек, а не выбор лучшей TP/SL-пары.`;
  expect(screen.queryByText(explanation)).not.toBeInTheDocument();
  const help = within(heading.parentElement!).getByRole("button", { name: "О лучшем score" });
  expect(heading.nextElementSibling).toContainElement(help);
  if (mode === "mean") fireEvent.mouseEnter(help.closest('[data-slot="tooltip-trigger"]')!);
  else if (mode === "forecast") act(() => { help.focus(); });
  else { fireEvent.pointerDown(help, { pointerType: "touch" }); fireEvent.click(help); }
  const tooltip = screen.getByText(explanation);
  expect(tooltip).toHaveAttribute("role", "tooltip");
  expect(help).toHaveAttribute("aria-describedby", tooltip.id);
  fireEvent.pointerDown(heading);
  expect(screen.queryByText(explanation)).not.toBeInTheDocument();
  expect(screen.getByRole("dialog", { name: "Оптимизация · Новый алгоритм" })).toBeInTheDocument();
  const reason = screen.getByText(`Причина завершения: ${partial ? "Явная остановка" : "Исчерпан бюджет времени"}.`);
  expect(reason.nextElementSibling).toHaveAttribute("data-slot", "separator");
  const parameters = screen.getByRole("region", { name: "Полные параметры результата" });
  expect(parameters.previousElementSibling).toHaveAttribute("data-slot", "separator");
  const pair = screen.getByRole("region", { name: "Лучшая forecast-пара" });
  const caption = "Максимальный чистый прогноз ячейки сохранённого победителя, не score всей матрицы. Среднее и TP/SL относятся к этой же ячейке.";
  expect(screen.queryByText(caption)).not.toBeInTheDocument();
  const pairHelp = within(pair).getByRole("button", { name: "О лучшей forecast-паре" });
  expect(within(pair).getByRole("heading").nextElementSibling).toContainElement(pairHelp);
  fireEvent.click(pairHelp);
  expect(screen.getByText(caption)).toHaveAttribute("role", "tooltip");
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it.each([[0.6, 0.174321, "40/60", "0.17 R"], [0.7, null, "30/70", "Не сохранено"]] as const)("shows only the concise saved blend caption for lambda %s and mean %s", async (lambda, mean, weights, formatted) => {
  const saved = winnerRecord();
  saved.result!.best = { ...saved.result!.best!, trendBlendLambda: lambda, meanScore: mean };
  studies = [saved]; render(<AlgorithmsPage />); await openOptimizer();
  expect(await screen.findByText(`Выбор по смеси среднего и прогноза (Смесь · ${weights}). Среднее всей матрицы за период: ${formatted}.`)).toBeInTheDocument();
  expect(screen.queryByText(/Прогноз не является средним за период/)).not.toBeInTheDocument();
  expect(saved.result!.best.score).toBe(0.23145202240242907);
  expect(saved.result!.best.meanScore).toBe(mean);
});

it.each(["mean", "forecast", "blend"] as const)("keeps the %s matrix collapsed, keyboard-focusable and bounded, with four-decimal metrics but integer counts", async (mode) => {
  const saved = winnerRecord(mode); const original = JSON.stringify(saved);
  studies = [saved]; render(<AlgorithmsPage />); await openOptimizer();
  await screen.findByRole("heading", { name: "Лучший score: 0.23 R" });
  const disclosure = screen.getByText("Матрица и поддержка результата");
  expect(disclosure.tagName).toBe("SUMMARY");
  expect(disclosure).toHaveClass("cursor-pointer", "font-medium");
  expect(disclosure.parentElement).not.toHaveAttribute("open");
  expect(screen.getByRole("table", { name: "Матрица и поддержка результата" })).not.toBeVisible();
  act(() => { disclosure.focus(); }); expect(disclosure).toHaveFocus();
  await userEvent.tab(); expect(screen.getByText("Все входы исследования")).toHaveFocus();
  await userEvent.tab({ shift: true }); expect(disclosure).toHaveFocus();
  // jsdom does not synthesize native summary keyboard activation; keep the
  // browser-owned details/summary contract, as with "Все входы исследования".
  await userEvent.click(disclosure);
  const table = screen.getByRole("table", { name: "Матрица и поддержка результата" });
  expect(disclosure.parentElement).toHaveAttribute("open");
  expect(table).toBeVisible();
  expect(table.parentElement).toHaveClass("overflow-x-auto");
  expect(within(table).getAllByRole("columnheader").map((cell) => cell.textContent)).toEqual([
    "TP %", "SL %", mode === "blend" ? "Blend R" : mode === "forecast" ? "Прогноз R" : "R",
    ...(mode !== "mean" ? ["Среднее R за период"] : []), ...(mode === "blend" ? ["Чистый прогноз R"] : []),
    "Среднее %", "Закрытых", "TP", "SL", "Не закрыты", "Нет данных", "Гейт исключил",
  ]);
  const cells = within(within(table).getAllByRole("row")[1]!).getAllByRole("cell").map((cell) => cell.textContent);
  expect(cells).toEqual(["4.0000", "6.0000", "0.2315", ...(mode !== "mean" ? ["0.1743"] : []), ...(mode === "blend" ? ["0.2695"] : []),
    "-0.1235", "100000", "65000", "35000", "2", "1", "0"]);
  expect(JSON.stringify(saved)).toBe(original);
  await userEvent.click(disclosure);
  expect(disclosure.parentElement).not.toHaveAttribute("open");
  expect(table).not.toBeVisible();
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it("preserves unavailable matrix metrics as dashes and real numeric zero as four decimals", async () => {
  const saved = winnerRecord();
  saved.result!.best!.matrix = [{ ...saved.result!.best!.matrix[0]!, riskExpectancy: 0, avgMovePct: 0, meanRiskExpectancy: null, forecastRiskExpectancy: null }];
  studies = [saved]; render(<AlgorithmsPage />); await openOptimizer();
  await screen.findByRole("heading", { name: "Лучший score: 0.23 R" });
  await userEvent.click(screen.getByText("Матрица и поддержка результата"));
  const table = screen.getByRole("table", { name: "Матрица и поддержка результата" });
  const cells = within(within(table).getAllByRole("row")[1]!).getAllByRole("cell");
  expect(cells.slice(0, 6).map((cell) => cell.textContent)).toEqual(["4.0000", "6.0000", "0.0000", "—", "—", "0.0000"]);
  expect(cells.slice(6).map((cell) => cell.textContent)).toEqual(["100000", "65000", "35000", "2", "1", "0"]);
});

it("labels a forecast winner and keeps its recorded negative period mean separate", async () => {
  const saved = record("completed");
  saved.spec = { ...spec, trendMode: "forecast", trendBendDays: "7" };
  saved.result = { progress, reason: "trial_budget", best: {
    score: 0.5, meanScore: -0.3, trendMode: "forecast", trendBendDays: "7", partial: false,
    params: algorithm.params, analytics: { ...spec.analyticsParams },
    matrix: [{ tpPct: 4, slPct: 4, riskExpectancy: 0.5, meanRiskExpectancy: -0.3, avgMovePct: -1.2,
      closedCount: 10, tpCount: 4, slCount: 6, unclosedCount: 1, noDataCount: 0, priceChangeFilteredCount: 0 }],
    provenance: { optimizerVersion: "plan-a-3", seed: 42, snapshotHash: "forecast-fixture", capturedAt: saved.createdAt }, conclusions: [],
  } };
  saved.forecastBest = { forecastR: 0.5, meanR: -0.3, tpPct: 4, slPct: 4 };
  studies = [saved];
  render(<AlgorithmsPage />); await openOptimizer();
  expect(await screen.findByText("Лучший score: 0.50 R")).toBeInTheDocument();
  expect(screen.getByText("Прогноз по тренду · перегиб 7 дней")).toBeInTheDocument();
  expect(screen.getByText(/Среднее всей матрицы за период: -0.30 R/)).toBeInTheDocument();
  expect(screen.getByRole("table", { name: "Матрица и поддержка результата" })).not.toBeVisible();
  await userEvent.click(screen.getByText("Матрица и поддержка результата"));
  expect(screen.getByRole("columnheader", { name: "Прогноз R" })).toBeInTheDocument();
  expect(screen.getByRole("columnheader", { name: "Среднее R за период" })).toBeInTheDocument();
  const pair = within(screen.getByRole("region", { name: "Лучшая forecast-пара" }));
  expect(pair.getByText("0.50 R")).toBeInTheDocument();
  expect(pair.getByText("-0.30 R")).toBeInTheDocument();
  expect(pair.getByText("4% / 4%")).toBeInTheDocument();
});

it("shows an immutable mean result recipe and cutoff without fabricating an uncomputed pure forecast", async () => {
  applyInterfaceSettings({ timezone: "America/New_York" });
  const saved = record("completed");
  const forecast = { method: "decayed_probability", version: 1, halfLifeDays: 14, kappa: 10, priorPolicy: "gross_break_even" } as const;
  const observationCutoff = Date.parse("2026-11-15T00:00:00Z") / 1000;
  saved.spec = { ...spec, trendMode: "mean", forecast };
  saved.result = { progress, reason: "trial_budget", best: { score: 0.2, meanScore: 0.2, trendMode: "mean", forecast, observationCutoff,
    partial: false, params: algorithm.params, analytics: spec.analyticsParams, matrix: [],
    provenance: { optimizerVersion: "fixture", seed: 42, snapshotHash: "immutable-mean", capturedAt: saved.createdAt }, conclusions: [] } };
  saved.forecastBest = null;
  studies = [saved];
  drafts.future_algorithm = { shared: { scope: { forecast: { ...forecast, halfLifeDays: "999", kappa: "999" } } } };
  render(<AlgorithmsPage />); await openOptimizer();
  const fixed = within(await screen.findByRole("region", { name: "Фиксированные требования исследования" }));
  expect(fixed.getByText("Среднее за период")).toBeInTheDocument();
  expect(fixed.getByText(/H 14 дней · κ 10/)).toBeInTheDocument();
  expect(fixed.getByText("Cutoff C результата").nextElementSibling).toHaveTextContent("14.11.2026, 19:00:00 (America/New_York) · из сохранённого результата, не текущей формы");
  expect(fixed.queryByText(/999/)).not.toBeInTheDocument();
  const pair = within(screen.getByRole("region", { name: "Лучшая forecast-пара" }));
  expect(pair.getByText("—")).toBeInTheDocument();
  expect(pair.queryByText("0.00 R")).not.toBeInTheDocument();
});

it.each([0, -0.126])("opens the exact history ID and shows its server forecast pair (%s), not blend or maximum mean", async (forecastR) => {
  const saved = record("completed");
  saved.spec = { ...spec, trendMode: "blend", trendBlendLambda: 0.7 };
  saved.forecastBest = { forecastR, meanR: -0.7, tpPct: 6, slPct: 4 };
  saved.result = { progress, reason: "trial_budget", best: {
    score: 8, meanScore: 9, trendMode: "blend", partial: false, params: algorithm.params,
    analytics: spec.analyticsParams, matrix: [],
    provenance: { optimizerVersion: "fixture", seed: 42, snapshotHash: "saved-pair", capturedAt: saved.createdAt }, conclusions: [],
  } };
  studies = [saved];
  render(<AlgorithmsPage />);
  const table = within(await screen.findByRole("table", { name: "Исследования · Новый алгоритм" }));
  const trigger = await historyButton("Результат исследования 17", table);
  expect(within(trigger.closest("tr")!).getAllByRole("cell")[8]).toHaveTextContent(`${forecastR.toFixed(2)} R`);
  fireEvent.click(trigger);
  const dialog = within(await screen.findByRole("dialog", { name: "Исследование #17 · Новый алгоритм" }));
  const pair = within(await dialog.findByRole("region", { name: "Лучшая forecast-пара" }));
  expect(pair.getAllByRole("definition").map((item) => item.textContent)).toEqual([`${forecastR.toFixed(2)} R`, "-0.70 R", "6% / 4%"]);
  expect(dialog.getByText("Лучший score: 8.00 R")).toBeInTheDocument();
  expect(requests.filter((request) => /^\/v1\/optimization-studies\/\d+$/.test(request.path)).map((request) => request.path)).toEqual(["/v1/optimization-studies/17"]);
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it("keeps the table's saved optimization recipe unchanged while the current research form edits another mode, lambda and method", async () => {
  studies = [winnerRecord("blend")];
  render(<AlgorithmsPage />);
  const trigger = await historyButton("Результат исследования 17");
  const summary = within(trigger.closest("tr")!).getAllByRole("cell")[5]!;
  expect(summary).toHaveTextContent("Смесь · λ 0.6"); expect(summary).toHaveTextContent("Тренд · перегиб 30 дней");
  const original = summary.textContent;
  const dialog = within(await openOptimizer());
  fireEvent.click(await dialog.findByRole("button", { name: "Новое исследование" }));
  await dialog.findByLabelText("Baseline: Новый порог");
  fireEvent.click(dialog.getByRole("button", { name: "Смесь среднего и прогноза" }));
  fireEvent.change(dialog.getByLabelText("Доля прогноза λ"), { target: { value: "0.9" } });
  expect(dialog.getByLabelText("Метод прогноза")).toHaveValue("decayed_probability");
  fireEvent.change(dialog.getByLabelText("Память модели H, дней"), { target: { value: "7.5" } });
  fireEvent.click(dialog.getByRole("button", { name: "Прогноз" }));
  fireEvent.blur(dialog.getByLabelText("Память модели H, дней"));
  await waitFor(() => expect(drafts[algorithm.id]).toMatchObject({ shared: { scope: { trendMode: "forecast", trendBlendLambda: "0.9", forecast: { method: "decayed_probability", halfLifeDays: "7.5" } } } }));
  expect(summary.textContent).toBe(original);
  await userEvent.click(dialog.getByRole("button", { name: "Закрыть" }));
  expect(within(trigger.closest("tr")!).getAllByRole("cell")[5]).toBe(summary);
  expect(summary.textContent).toBe(original);
  expect(requests.some((request) => request.method === "POST")).toBe(false);
}, 15000);

it("submits blend lambda and forward gate, persists and restores them on reopen", async () => {
  render(<AlgorithmsPage />); let dialog = within(await openOptimizer());
  await dialog.findByLabelText("Baseline: Новый порог");
  fireEvent.click(dialog.getByRole("button", { name: "Смесь среднего и прогноза" }));
  expect(dialog.getByLabelText("Доля прогноза λ")).toHaveValue(0.7);
  expect(dialog.getByLabelText("Минимальный прогноз матрицы, R")).toHaveValue(0);
  fireEvent.change(dialog.getByLabelText("Доля прогноза λ"), { target: { value: "0.6" } });
  fireEvent.change(dialog.getByLabelText("Минимальный прогноз матрицы, R"), { target: { value: "0.2" } });
  fireEvent.blur(dialog.getByLabelText("Минимальный прогноз матрицы, R"));
  await waitFor(() => expect(drafts.future_algorithm).toMatchObject({ shared: { scope: { trendMode: "blend", trendBlendLambda: "0.6", minForecastExpectancy: "0.2" } } }));
  await userEvent.click(dialog.getByRole("button", { name: "Закрыть" }));
  dialog = within(await openOptimizer());
  expect(await dialog.findByLabelText("Доля прогноза λ")).toHaveValue(0.6);
  expect(dialog.getByLabelText("Минимальный прогноз матрицы, R")).toHaveValue(0.2);
  enterDates(); const start = dialog.getByRole("button", { name: "Начать исследование" });
  await waitFor(() => expect(start).toBeEnabled()); fireEvent.click(start); await screen.findByText("Исследование #17");
  expect(requests.find((request) => request.path === "/v1/optimization-studies" && request.method === "POST")?.body)
    .toMatchObject({ trendMode: "blend", trendBlendLambda: 0.6, minForecastExpectancy: 0.2 });
});

it("autosaves on blur, restores after reopening and isolates drafts by algorithm", async () => {
  render(<AlgorithmsPage />);
  let dialog = within(await openOptimizer());
  const baseline = await dialog.findByLabelText("Baseline: Новый порог");
  fireEvent.change(dialog.getByLabelText("Начало периода сигналов"), { target: { value: "2026-10-02T03:04" } });
  fireEvent.change(dialog.getByLabelText("Горизонт сделки, мин (фиксирован)"), { target: { value: "720" } });
  fireEvent.change(baseline, { target: { value: "8.75" } });
  fireEvent.blur(baseline);
  await waitFor(() => expect(requests.filter((request) => request.path === "/v1/optimization-drafts/future_algorithm" && request.method === "PUT")).toHaveLength(1));
  expect(drafts.future_algorithm).toMatchObject({
    version: 1,
    shared: { scope: { horizonMinutes: "720" }, algorithmDomains: { futureThreshold: { baseline: "8.75" } } },
    manual: { from: "2026-10-02T03:04" },
    continuous: { lookbackDays: "30" },
  });
  await userEvent.click(dialog.getByRole("button", { name: "Закрыть" }));
  dialog = within(await openOptimizer());
  expect(await dialog.findByLabelText("Начало периода сигналов")).toHaveValue("2026-10-02T03:04");
  expect(dialog.getByLabelText("Горизонт сделки, мин (фиксирован)")).toHaveValue(720);
  expect(dialog.getByLabelText("Baseline: Новый порог")).toHaveValue("8.75");
  await userEvent.click(dialog.getByRole("button", { name: "Закрыть" }));
  await userEvent.click(screen.getByRole("tab", { name: "Второй алгоритм" }));
  await userEvent.click(screen.getByRole("button", { name: "Запустить новое исследование Второй алгоритм" }));
  const second = within(await screen.findByRole("dialog", { name: "Оптимизация · Второй алгоритм" }));
  expect(await second.findByLabelText("Горизонт сделки, мин (фиксирован)")).toHaveValue(10080);
  expect(second.getByLabelText("Baseline: Новый порог")).toHaveValue("2.5");
  expect(drafts.second_algorithm).toBeUndefined();
});

it("shows a draft autosave failure without closing the optimizer", async () => {
  draftStatus = 503;
  render(<AlgorithmsPage />);
  const dialog = within(await openOptimizer());
  const input = await dialog.findByLabelText("Бюджет trials");
  fireEvent.change(input, { target: { value: "128" } });
  fireEvent.blur(input);
  expect(await dialog.findByRole("alert")).toHaveTextContent("HTTP 503");
  expect(screen.getByRole("dialog", { name: "Оптимизация · Новый алгоритм" })).toBeInTheDocument();
  expect(requests.some((request) => request.method === "POST")).toBe(false);
});

it("renders ungrouped metadata in normal settings and uses its saved values as the optimizer baseline", async () => {
  render(<AlgorithmsPage />);
  const heading = await screen.findByText("Новый алгоритм · 2.0");
  const settings = within(heading.closest("form")!);
  fireEvent.click(settings.getByRole("tab", { name: "Live-сканирование с постоянными параметрами" }));
  const threshold = settings.getByLabelText("Новый порог (%)");
  const window = settings.getByLabelText("Новое окно (мин)");
  expect(threshold).toHaveValue(2.5);
  expect(window).toHaveAttribute("min", "1");
  expect(window).toHaveAttribute("max", "50");
  expect(window).toHaveAttribute("step", "1");
  expect(within(settings.getByRole("group", { name: "Другие параметры" })).getByLabelText("Новое окно (мин)")).toBe(window);
  fireEvent.change(threshold, { target: { value: "3.75" } });
  fireEvent.change(window, { target: { value: "7" } });
  fireEvent.click(settings.getByRole("button", { name: "Сохранить алгоритм" }));
  await settings.findByText("Настройки алгоритма сохранены.");
  expect(requests.find((request) => request.path === `/v1/algorithms/${algorithm.id}`)?.body).toEqual({ params: { futureThreshold: "3.75", futureWindow: 7 }, enabled: true });
  const optimizer = within(await openOptimizer());
  expect(await optimizer.findByLabelText("Baseline: Новый порог")).toHaveValue("3.75");
  expect(optimizer.getByLabelText("Baseline: Новое окно")).toHaveValue("7");
  expect(optimizer.getByLabelText("Подбирать: Новое окно")).not.toBeChecked();
  enterDates();
  const start = optimizer.getByRole("button", { name: "Начать исследование" });
  await waitFor(() => expect(start).toBeEnabled()); fireEvent.click(start);
  await screen.findByText("Исследование #17");
  expect(requests.find((request) => request.path === "/v1/optimization-studies" && request.method === "POST")?.body).toMatchObject({ algorithmParams: { futureThreshold: "3.75", futureWindow: 7 }, algorithmSearch: { futureThreshold: ["1.5", "2.5", "4"] } });
});

it("restores the same server study after remount without starting or cancelling it", async () => {
  studies = [record()];
  const first = render(<AlgorithmsPage />);
  await openOptimizer();
  expect(await screen.findByText("Исследование #17")).toBeInTheDocument();
  expect(screen.getByText("Подготовка снимка данных")).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "Закрыть" }));
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  await waitFor(() => expect(screen.getByRole("button", { name: "Запустить новое исследование Новый алгоритм" })).toHaveFocus());
  await openOptimizer();
  expect(screen.getByText("Исследование #17")).toBeInTheDocument();
  first.unmount();
  expect(requests.every((request) => request.method === "GET" || request.path.startsWith("/v1/optimization-drafts/"))).toBe(true);
  render(<AlgorithmsPage />);
  await openOptimizer();
  expect(await screen.findByText("Исследование #17")).toBeInTheDocument();
  expect(requests.filter((request) => request.path === "/v1/optimization-studies/17").length).toBeGreaterThanOrEqual(2);
  expect(requests.every((request) => request.method === "GET" || request.path.startsWith("/v1/optimization-drafts/"))).toBe(true);
});

it("explicitly requests Stop once and observes cancellation until the server is terminal", async () => {
  studies = [record()];
  render(<AlgorithmsPage />);
  await openOptimizer();
  const stop = await screen.findByRole("button", { name: "Остановить исследование" });
  fireEvent.click(stop); fireEvent.click(stop);
  expect(await screen.findByText("Отмена запрошена")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Новое исследование" })).not.toBeInTheDocument();
  expect(requests.filter((request) => request.path.endsWith("/cancel"))).toHaveLength(1);
  studies = [{ ...record("cancelled"), result: { progress, best: null, reason: "cancelled" } }];
  await userEvent.click(screen.getByRole("button", { name: "Обновить статус" }));
  expect(await screen.findByText("Отменено")).toBeInTheDocument();
  expect(screen.getByText(/Нет оценимого кандидата/)).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Новое исследование" })).toBeEnabled();
});

it.each(["", "0, 4", "101", "4, 4", "NaN", Array.from({ length: 65 }, (_, i) => String(i + 1)).join(",")])("rejects invalid independent TP axes before creation: %s", async (levels) => {
  render(<AlgorithmsPage />);
  await openOptimizer(); enterDates();
  fireEvent.change(screen.getByLabelText("Уровни TP, %"), { target: { value: levels } });
  await userEvent.click(screen.getByRole("button", { name: "Начать исследование" }));
  expect(await screen.findByRole("alert")).toHaveTextContent(/TP/);
  expect(requests.every((request) => request.method === "GET" || request.path.startsWith("/v1/optimization-drafts/"))).toBe(true);
});

it("handles a cross-client 409 by discovering the active ID instead of retrying creation", async () => {
  createStatus = 409;
  render(<AlgorithmsPage />);
  await openOptimizer(); enterDates();
  await userEvent.click(screen.getByRole("button", { name: "Начать исследование" }));
  expect(await screen.findByText("Исследование #17")).toBeInTheDocument();
  expect(screen.getByRole("alert")).toHaveTextContent(/уже выполняется/);
  expect(requests.filter((request) => request.method === "POST")).toHaveLength(1);
});

it.each([false, true])("restores full terminal inputs with exact UI labels and clustering %s without applying settings", async (clusterEnabled) => {
  const best = {
    score: 0.125, partial: false, params: { futureThreshold: "4", futureWindow: 5 },
    analytics: { clusterEnabled, clusterGapMinutes: clusterEnabled ? 480 : null, timeShiftMinutes: 5, priceChangePct: null },
    matrix: [{ tpPct: 4, slPct: 4, riskExpectancy: 0.125, avgMovePct: 0.5, closedCount: 8, tpCount: 5, slCount: 3, unclosedCount: 2, noDataCount: 1, priceChangeFilteredCount: 4 }],
    ...(clusterEnabled ? { support: { selectedSetupCount: 9, avgSetupsPerDay: null } } : {}),
    provenance: { optimizerVersion: "plan-a-1", seed: 42, snapshotHash: "snapshot-test-hash", capturedAt: "2026-11-10T00:00:00Z" }, conclusions: ["Проверенный вывод"],
  };
  studies = [{ ...record("completed"), ...(clusterEnabled ? { forecastBest: null } : {}), result: { progress: { ...progress, bestScore: best.score }, best, reason: "trial_budget" } }];
  render(<AlgorithmsPage />);
  await openOptimizer();
  expect(await screen.findByText("Лучший score: 0.13 R")).toBeInTheDocument();
  expect(screen.getByText("snapshot-test-hash")).toBeInTheDocument();
  expect(within(screen.getByRole("region", { name: "Лучшая forecast-пара" })).getByText("—")).toBeInTheDocument();
  await userEvent.click(screen.getByText("Матрица и поддержка результата"));
  expect(screen.getByRole("table", { name: "Матрица и поддержка результата" })).toHaveTextContent("Закрытых");
  await userEvent.click(screen.getByRole("button", { name: "О лучшем score" }));
  expect(screen.getByRole("tooltip")).toHaveTextContent(/не доказанный глобальный оптимум/);
  fireEvent.pointerDown(screen.getByRole("heading", { name: "Лучший score: 0.13 R" }));
  const requirements = within(screen.getByRole("region", { name: "Фиксированные требования исследования" }));
  expect(requirements.getByText("Гейт не применялся")).toBeInTheDocument();
  expect(screen.queryByText("Среднее сетапов в день")).not.toBeInTheDocument();
  const support = within(screen.getByRole("region", { name: "Поддержка результата" }));
  expect(support.getByText("Выбрано сетапов").nextElementSibling).toHaveTextContent(clusterEnabled ? "9" : "Не сохранено");
  const parameters = within(screen.getByRole("region", { name: "Полные параметры результата" }));
  expect(parameters.getAllByRole("term").map((term) => term.textContent)).toEqual([
    "Новый порог (%)", "Новое окно (мин)", "Кластеризовать сетапы", "Разрыв кластера, ч",
    "Time-shift, мин", "Смещение цены, %", "Зрелость сетапа в кластере", "Горизонт сделки, мин (фиксирован)",
  ]);
  expect(parameters.getAllByRole("definition").map((value) => value.textContent)).toEqual([
    "4", "5", String(clusterEnabled), clusterEnabled ? "8" : "null", "5", "null", clusterEnabled ? "0" : "null", "10080",
  ]);
  await userEvent.click(screen.getByText("Все входы исследования"));
  expect(screen.getByLabelText("Сохранённый spec")).toHaveTextContent("algorithmSearch");
  expect(screen.getByLabelText("Сохранённый spec")).not.toHaveTextContent("minSetupsPerDay");
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it.each([0.6, 0])("renders fixed frequency threshold %s and actual winner support without recalculating it", async (minSetupsPerDay) => {
  const best = {
    score: 0.125, partial: false, params: algorithm.params,
    analytics: { clusterEnabled: true, clusterGapMinutes: 480, timeShiftMinutes: 0, priceChangePct: null },
    matrix: [], support: { selectedSetupCount: 6, avgSetupsPerDay: 0.6, requiredSetupCount: minSetupsPerDay === 0 ? 0 : 6 },
    provenance: { optimizerVersion: "plan-a-2", seed: 42, snapshotHash: "frequency-snapshot", capturedAt: "2026-11-10T00:00:00Z" }, conclusions: [],
  };
  studies = [{ ...record("completed"), spec: { ...spec, minSetupsPerDay }, result: { progress, best, reason: "trial_budget" } }];
  render(<AlgorithmsPage />);
  await openOptimizer();
  const requirements = within(await screen.findByRole("region", { name: "Фиксированные требования исследования" }));
  expect(requirements.getByText("Минимум сетапов в день").nextElementSibling).toHaveTextContent(String(minSetupsPerDay));
  expect(requirements.queryByText("Гейт не применялся")).not.toBeInTheDocument();
  if (minSetupsPerDay === 0) expect(requirements.getByText(/гейт отключён/)).toBeInTheDocument();
  const support = within(screen.getByRole("region", { name: "Поддержка результата" }));
  expect(support.getByText("Выбрано сетапов").nextElementSibling).toHaveTextContent("6");
  expect(support.getByText("Среднее сетапов в день").nextElementSibling).toHaveTextContent("0.6");
  expect(screen.getByLabelText("Сохранённый spec")).toHaveTextContent(`"minSetupsPerDay": ${minSetupsPerDay}`);
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it.each([undefined, 0, 4])("explains only applied gates when no candidate is scorable with threshold %s", async (minSetupsPerDay) => {
  studies = [{ ...record("completed"), spec: { ...spec, ...(minSetupsPerDay === undefined ? {} : { minSetupsPerDay }) }, result: { progress, best: null, reason: "space_exhausted" } }];
  render(<AlgorithmsPage />);
  await openOptimizer();
  const warning = await screen.findByText(/Нет оценимого кандидата/);
  expect(warning).toHaveTextContent("Минимум закрытых на ячейку");
  if (minSetupsPerDay !== undefined && minSetupsPerDay > 0) expect(warning).toHaveTextContent("Минимум сетапов в день");
  else expect(warning).not.toHaveTextContent("Минимум сетапов в день");
});

it("pauses observer polling and releases requests/timers on unmount without cancelling server work", async () => {
  vi.useFakeTimers(); studies = [record()];
  const view = render(<AlgorithmsPage />);
  await act(async () => { await vi.advanceTimersByTimeAsync(0); });
  const before = requests.length;
  await act(async () => { await vi.advanceTimersByTimeAsync(3000); });
  expect(requests.length).toBeGreaterThan(before);
  view.unmount();
  const after = requests.length;
  await act(async () => { await vi.advanceTimersByTimeAsync(9000); });
  expect(requests).toHaveLength(after);
  expect(requests.every((request) => request.method === "GET")).toBe(true);
  expect(vi.getTimerCount()).toBe(0);
});

it("discovers compact summaries without loading matrices until a dialog opens and releases detail on close", async () => {
  vi.useFakeTimers(); studies = [record("completed")];
  render(<AlgorithmsPage />);
  await act(async () => { await vi.advanceTimersByTimeAsync(0); });
  const lists = requests.filter((request) => request.path.startsWith("/v1/optimization-studies?"));
  expect(lists.filter((request) => new URL(request.path, "http://localhost").searchParams.get("limit") === "1")).toHaveLength(1);
  // Two retained algorithm discovery summaries plus the selected inline page.
  expect(lists.filter((request) => new URL(request.path, "http://localhost").searchParams.get("limit") === "20")).toHaveLength(3);
  expect(lists.filter((request) => new URL(request.path, "http://localhost").searchParams.get("limit") === "5")).toHaveLength(0);
  expect(lists.filter((request) => new URL(request.path, "http://localhost").searchParams.get("limit") === "10")).toHaveLength(0);
  expect(requests.filter((request) => request.path === "/v1/optimization-studies/17")).toHaveLength(0);
  fireEvent.click(screen.getByRole("button", { name: "Запустить новое исследование Новый алгоритм" }));
  await act(async () => { await vi.advanceTimersByTimeAsync(0); });
  expect(requests.filter((request) => request.path === "/v1/optimization-studies/17")).toHaveLength(1);
  await act(async () => { await vi.advanceTimersByTimeAsync(9000); });
  expect(requests.filter((request) => request.path === "/v1/optimization-studies/17")).toHaveLength(1);
  studies = [{ ...studies[0]!, updatedAt: "2026-11-10T00:02:00Z" }];
  await act(async () => { await vi.advanceTimersByTimeAsync(6000); });
  expect(requests.filter((request) => request.path === "/v1/optimization-studies/17")).toHaveLength(2);
  fireEvent.click(screen.getByRole("button", { name: "Закрыть" }));
  await act(async () => { await vi.advanceTimersByTimeAsync(9000); });
  expect(requests.filter((request) => request.path === "/v1/optimization-studies/17")).toHaveLength(2);
});

it("reserves twenty skeleton rows, column widths and pagination throughout initial history loading", async () => {
  vi.useFakeTimers(); studies = [record("completed")];
  const original = globalThis.fetch;
  let discovery = true;
  let finish: (() => void) | undefined;
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    const params = new URL(path, "http://localhost").searchParams;
    if (params.get("algorithmId") === algorithm.id && params.get("limit") === "20") {
      if (discovery) discovery = false;
      else if (finish === undefined) return new Promise<Response>((resolve) => { finish = () => { void original(path, init).then(resolve); }; });
    }
    return original(path, init);
  }));
  render(<AlgorithmsPage />); await act(async () => { await vi.advanceTimersByTimeAsync(0); });
  const table = screen.getByRole("table", { name: "Исследования · Новый алгоритм" });
  expect(finish).toBeDefined(); expect(table).toHaveAttribute("aria-busy", "true");
  expect(within(table).getAllByRole("columnheader")).toHaveLength(11);
  expect(table.querySelectorAll("tbody tr")).toHaveLength(20);
  expect(table.querySelectorAll('[data-slot="skeleton"]')).toHaveLength(220);
  const next = screen.getByRole("button", { name: /Следующая страница исследований/ });
  expect(next).toBeDisabled(); expect(next.parentElement).toHaveClass("min-h-8");
  await act(async () => { finish?.(); await vi.advanceTimersByTimeAsync(0); });
  expect(screen.getByRole("button", { name: "Результат исследования 17" })).toBeInTheDocument();
  expect(table.querySelectorAll("tbody tr")).toHaveLength(20);
  expect(table.querySelectorAll('[data-slot="skeleton"]')).toHaveLength(0);
  expect(screen.getByRole("button", { name: /Следующая страница исследований/ })).toBe(next);
});

it("shows the full inline history in server pages of twenty, preserving summary formatting and a short last page's height", async () => {
  studies = Array.from({ length: 25 }, (_, index) => ({ ...record("completed"), id: 50 - index,
    progress: { ...progress, elapsedSeconds: index === 1 ? 90061 : 12, bestScore: index === 0 ? 0 : index === 1 ? null : index === 3 ? -0.126 : index === 4 ? 1e21 : 0.25 },
    spec: { ...spec, ...(index === 0 ? { minSetupsPerDay: 0 } : index > 1 ? { minSetupsPerDay: 4 } : {}) },
    ...(index !== 1 ? { forecastBest: { forecastR: index === 0 ? 0 : index === 3 ? -0.126 : 0.876, meanR: -0.7, tpPct: 6, slPct: 4 } } : {}),
  }));
  render(<AlgorithmsPage />);
  const element = await screen.findByRole("table", { name: "Исследования · Новый алгоритм" });
  const table = within(element);
  await historyButton("Результат исследования 50", table);
  expect(screen.queryByText("Загрузка истории исследований…")).not.toBeInTheDocument();
  expect(table.getAllByRole("columnheader").map((header) => header.textContent)).toEqual(["ID", "Происхождение", "Диапазон дат", "Время исследования", "Минимум сетапов в день", "Оптимизация матожидания", "Лучшая ячейка TP/SL", "Лучший Score матрицы", "Лучший прогноз для ячейки", "Матожидание факт", "Действия"]);
  expect(table.getAllByRole("row")).toHaveLength(21);
  const first = within(table.getAllByRole("row")[1]!);
  expect(first.getAllByRole("cell").map((cell) => cell.textContent)).toEqual(["#50", "Ручное", `${formatUnix(spec.fromUnix)} -${formatUnix(spec.toUnix)}`, "Менее 1 мин", "0", "Среднее", "TP 6% / SL 4%", "0.00 R", "0.00 R", "Не рассчитано—", ""]);
  expect(table.getAllByRole("row")[2]).toHaveTextContent("Гейт не применялся");
  expect(table.getAllByRole("row")[2]).toHaveTextContent("—");
  expect(table.getAllByRole("row")[2]).toHaveTextContent("1 дн 1 ч 1 мин");
  expect(table.getAllByRole("row")[3]).toHaveTextContent("0.25 R");
  expect(within(table.getAllByRole("row")[2]!).getAllByRole("cell")[6]).toHaveTextContent(/^—$/);
  expect(within(table.getAllByRole("row")[2]!).getAllByRole("cell")[8]).toHaveTextContent(/^—$/);
  expect(within(table.getAllByRole("row")[3]!).getAllByRole("cell")[8]).toHaveTextContent("0.88 R");
  expect(table.getAllByRole("row")[4]).toHaveTextContent("-0.13 R");
  expect(table.getAllByRole("row")[5]).toHaveTextContent("1000000000000000000000.00 R");
  const section = within(screen.getByRole("region", { name: "История оптимизации · Новый алгоритм" }));
  expect(section.getByRole("button", { name: /Предыдущая страница/ })).toBeDisabled();
  expect(element).toHaveClass("table-fixed", "min-w-7xl");
  expect(element.querySelectorAll("colgroup col")).toHaveLength(11);
  expect(element.querySelector("tbody")).toHaveClass("[&_td]:py-1.5", "[&_td]:align-middle");
  expect(element.querySelector("tbody")).not.toHaveClass("[&_tr]:h-20");
  expect(element.parentElement).toHaveClass("min-h-[68.25rem]");
  expect(table.getAllByRole("row")[1]).not.toHaveClass("h-20", "h-13");
  expect(element.querySelector(".max-h-14")).toBeNull();
  const ids = [...document.querySelectorAll("[id]")].map((element) => element.id);
  expect(new Set(ids).size).toBe(ids.length);
  fireEvent.click(section.getByRole("button", { name: /Следующая страница/ }));
  expect(screen.queryByText("Загрузка истории исследований…")).not.toBeInTheDocument();
  expect(element).toHaveAttribute("aria-busy", "true");
  expect(element.querySelectorAll('[data-slot="skeleton"]')).toHaveLength(220);
  expect(section.getByRole("button", { name: /Следующая страница/ })).toBeDisabled();
  await historyButton("Результат исследования 30", table);
  expect(table.getAllByRole("row")).toHaveLength(6);
  expect(element.querySelectorAll("tbody tr")).toHaveLength(20);
  expect(element.querySelectorAll('tbody tr[aria-hidden="true"]')).toHaveLength(15);
  expect([...element.querySelectorAll('tbody tr[aria-hidden="true"]')].every((row) => row.classList.contains("h-13"))).toBe(true);
  expect([...element.querySelectorAll('tbody tr[aria-hidden="true"] td')].every((cell) => cell.getAttribute("colspan") === "11")).toBe(true);
  expect(element.parentElement).toHaveClass("min-h-[68.25rem]");
  expect(section.getByRole("button", { name: /Следующая страница/ })).toBeDisabled();
  expect(requests.some((request) => new URL(request.path, "http://localhost").searchParams.get("offset") === "20")).toBe(true);
  expect(requests.filter((request) => /limit=(5|10)&/.test(request.path))).toHaveLength(0);
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(requests.some((request) => /^\/v1\/optimization-studies\/\d+$/.test(request.path))).toBe(false);
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it("keeps the inline page underneath exact-ID detail and restores its page and unique trigger focus", async () => {
  studies = Array.from({ length: 25 }, (_, index) => ({ ...record("completed"), id: 50 - index }));
  render(<AlgorithmsPage />);
  const card = within((await screen.findByText("Новый алгоритм · 2.0")).closest("form")!);
  const listDialog = card.getByRole("region", { name: "История оптимизации · Новый алгоритм" });
  const list = within(listDialog);
  await historyButton("Результат исследования 50", list);
  await userEvent.click(list.getByRole("button", { name: /Следующая страница/ }));
  const idTrigger = await historyButton("Результат исследования 29", list);
  await userEvent.click(idTrigger);
  const detail = within(await screen.findByRole("dialog", { name: "Исследование #29 · Новый алгоритм" }));
  expect(await detail.findByText("Исследование #29")).toBeInTheDocument();
  expect(listDialog).toBeInTheDocument();
  expect(document.querySelectorAll('[data-slot="dialog-content"]')).toHaveLength(1);
  await userEvent.click(detail.getByRole("button", { name: "Закрыть" }));
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  await waitFor(() => expect(list.getByRole("button", { name: "Результат исследования 29" })).toBe(idTrigger));
  await waitFor(() => expect(idTrigger).toHaveFocus());
  expect(list.getByText("Страница 2 · всего 25")).toBeInTheDocument();
  expect(requests.filter((request) => /^\/v1\/optimization-studies\/\d+$/.test(request.path)).map((request) => request.path)).toEqual(["/v1/optimization-studies/29"]);
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it("loads only the selected algorithm's current inline page and keeps same-page active rows and focus mounted", async () => {
  vi.useFakeTimers(); studies = [record()];
  render(<AlgorithmsPage />);
  await act(async () => { await vi.advanceTimersByTimeAsync(0); });
  const pages = () => requests.filter((request) => new URL(request.path, "http://localhost").searchParams.get("limit") === "20");
  expect(pages()).toHaveLength(3);
  const trigger = screen.getByRole("button", { name: "Результат исследования 17" });
  act(() => { trigger.focus(); });
  const inlineBefore = pages().length;
  await act(async () => { await vi.advanceTimersByTimeAsync(9000); });
  expect(pages().length).toBeGreaterThan(inlineBefore);
  expect(pages().filter((request) => new URL(request.path, "http://localhost").searchParams.get("algorithmId") === algorithm.id).length).toBeGreaterThan(1);
  expect(screen.getByRole("button", { name: "Результат исследования 17" })).toBe(trigger);
  expect(trigger).toHaveFocus();
  expect(screen.getByRole("table").querySelectorAll('[data-slot="skeleton"]')).toHaveLength(0);
  expect(requests.some((request) => /^\/v1\/optimization-studies\/\d+$/.test(request.path))).toBe(false);
});

it("keeps shared discovery fast for a hidden algorithm's running process between children without losing fixed-form state", async () => {
  vi.useFakeTimers();
  const original = globalThis.fetch;
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    if (path.startsWith("/v1/continuous-studies?")) {
      requests.push({ path, method: "GET", body: null, ...(init?.signal ? { signal: init.signal } : {}) });
      const id = new URL(path, "http://localhost").searchParams.get("algorithmId");
      return Promise.resolve(new Response(JSON.stringify({ processes: [{ algorithmId: id, status: id === "second_algorithm" ? "running" : "stopped", configRevision: 1, config: null, activeStudyId: null, phase: id === "second_algorithm" ? "queued" : null, error: null, slots: [] }] })));
    }
    return original(path, init);
  }));
  render(<AlgorithmsPage />);
  await act(async () => { await vi.advanceTimersByTimeAsync(0); });
  fireEvent.click(screen.getByRole("tab", { name: "Второй алгоритм" }));
  await act(async () => { await vi.advanceTimersByTimeAsync(0); });
  fireEvent.click(screen.getByRole("tab", { name: "Новый алгоритм" }));
  await act(async () => { await vi.advanceTimersByTimeAsync(0); });
  expect(screen.getByRole("tab", { name: "Новый алгоритм" })).toHaveAttribute("aria-selected", "true");
  fireEvent.click(screen.getByRole("tab", { name: "Live-сканирование с постоянными параметрами" }));
  const fixed = screen.getByRole("spinbutton", { name: "Новый порог (%)" });
  fireEvent.change(fixed, { target: { value: "8.5" } });
  const globalReads = () => requests.filter((request) => request.path === "/v1/optimization-studies?limit=1&offset=0").length;
  const before = globalReads();
  await act(async () => { await vi.advanceTimersByTimeAsync(3000); });
  expect(globalReads()).toBeGreaterThan(before);
  expect(screen.getByRole("spinbutton", { name: "Новый порог (%)" })).toBe(fixed);
  expect(fixed).toHaveValue(8.5);
  fireEvent.click(screen.getByRole("tab", { name: "Второй алгоритм" }));
  fireEvent.click(screen.getByRole("tab", { name: "Новый алгоритм" }));
  expect(screen.getByRole("spinbutton", { name: "Новый порог (%)" })).toHaveValue(8.5);
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it("corrects inline pagination after deletion and restores the underlying list after detail closes", async () => {
  studies = Array.from({ length: 21 }, (_, index) => ({ ...record("completed"), id: 50 - index }));
  render(<AlgorithmsPage />);
  const card = within((await screen.findByText("Новый алгоритм · 2.0")).closest("form")!);
  const listDialog = card.getByRole("region", { name: "История оптимизации · Новый алгоритм" });
  const list = within(listDialog);
  await historyButton("Результат исследования 50", list);
  await userEvent.click(list.getByRole("button", { name: /Следующая страница/ }));
  await userEvent.click(await historyButton("Результат исследования 30", list));
  await screen.findByText("Исследование #30");
  await userEvent.click(screen.getByRole("button", { name: "Удалить исследование 30" }));
  const confirmation = within(await screen.findByRole("dialog", { name: "Удалить исследование #30?" }));
  await userEvent.click(confirmation.getByRole("button", { name: "Удалить" }));
  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  await list.findByText("Страница 1 · всего 20");
  await waitFor(() => expect(listDialog).toHaveFocus());
  expect(list.queryByRole("button", { name: "Результат исследования 30" })).not.toBeInTheDocument();
  expect(list.getByRole("button", { name: /Предыдущая страница/ })).toBeDisabled();
  expect(list.getByRole("button", { name: /Следующая страница/ })).toBeDisabled();
  expect(requests.filter((request) => request.method === "DELETE").map((request) => request.path)).toEqual(["/v1/optimization-studies/30"]);
});

it("preserves stop and pending actions in the inline history", async () => {
  studies = [record()];
  render(<AlgorithmsPage />);
  const card = within((await screen.findByText("Новый алгоритм · 2.0")).closest("form")!);
  const list = within(card.getByRole("region", { name: "История оптимизации · Новый алгоритм" }));
  const stop = await historyButton("Остановить исследование 17", list);
  fireEvent.click(stop); fireEvent.click(stop);
  await list.findByText("Остановка запрошена");
  expect(list.getByRole("button", { name: "Остановить исследование 17" })).toBeDisabled();
  expect(requests.filter((request) => request.path.endsWith("/cancel"))).toHaveLength(1);
  expect(requests.some((request) => request.method === "PUT")).toBe(false);
});

it("shows a stop mutation error above the inline history", async () => {
  studies = [record()]; cancelStatus = 503;
  render(<AlgorithmsPage />);
  const card = within((await screen.findByText("Новый алгоритм · 2.0")).closest("form")!);
  const list = within(card.getByRole("region", { name: "История оптимизации · Новый алгоритм" }));
  await userEvent.click(await historyButton("Остановить исследование 17", list));
  const manual = within(card.getByRole("heading", { name: "Исследования оптимальных параметров" }).closest("section")!);
  expect(await manual.findByRole("alert")).toHaveTextContent("HTTP 503");
  expect(list.getByRole("button", { name: "Остановить исследование 17" })).toBeEnabled();
  expect(requests.filter((request) => request.path.endsWith("/cancel"))).toHaveLength(1);
});

it("opens the requested older ID independently from latest discovery without nested optimizer dialogs", async () => {
  studies = [{ ...record("completed"), id: 20 }, { ...record("cancelled"), id: 17 }];
  render(<AlgorithmsPage />);
  fireEvent.click(await historyButton("Результат исследования 17"));
  expect(await screen.findByText("Исследование #17")).toBeInTheDocument();
  expect(screen.getAllByRole("dialog")).toHaveLength(1);
  studies.unshift({ ...record("completed"), id: 21 });
  const readsBefore = requests.filter((request) => request.path.startsWith("/v1/optimization-studies?")).length;
  expect(screen.queryByRole("button", { name: "Обновить статус" })).not.toBeInTheDocument();
  fireEvent(document, new Event("visibilitychange"));
  await waitFor(() => expect(requests.filter((request) => request.path.startsWith("/v1/optimization-studies?")).length).toBeGreaterThan(readsBefore));
  expect(screen.getByText("Исследование #17")).toBeInTheDocument();
  expect(requests.some((request) => request.path === "/v1/optimization-studies/20" || request.path === "/v1/optimization-studies/21")).toBe(false);
  await userEvent.click(screen.getByRole("button", { name: "Закрыть" }));
  fireEvent.click(await historyButton("Результат исследования 17"));
  expect(await screen.findByText("Исследование #17")).toBeInTheDocument();
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it.each(["queued", "running"] as const)("stops an active %s history row once without submitting settings", async (status) => {
  studies = [record(status)];
  render(<AlgorithmsPage />);
  const stop = await historyButton("Остановить исследование 17");
  fireEvent.click(stop); fireEvent.click(stop);
  await waitFor(() => expect(screen.getByRole("button", { name: "Остановить исследование 17" })).toBeDisabled());
  expect(await screen.findByText("Остановка запрошена")).toBeInTheDocument();
  expect(requests.filter((request) => request.path.endsWith("/cancel"))).toHaveLength(1);
  expect(requests.some((request) => request.method === "PUT")).toBe(false);
});

it("does not offer repeated stop or deletion for cancel_requested", async () => {
  studies = [record("cancel_requested")];
  render(<AlgorithmsPage />);
  const stop = await historyButton("Остановить исследование 17");
  expect(stop).toBeDisabled(); fireEvent.click(stop);
  expect(screen.queryByRole("button", { name: "Удалить исследование 17" })).not.toBeInTheDocument();
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it.each(["completed", "cancelled", "failed"] as const)("confirms terminal %s deletion, closes selected detail and refreshes history", async (status) => {
  studies = [{ ...record("completed"), id: 20 }, record(status)];
  render(<AlgorithmsPage />);
  // Initial mount + discovery + history are distinct async steps; the 1s query
  // default can expire before history is requested on the larger Algorithms page.
  fireEvent.click(await historyButton("Результат исследования 17"));
  await screen.findByText("Исследование #17");
  fireEvent.click(screen.getByRole("button", { name: "Удалить исследование 17" }));
  const confirmation = within(await screen.findByRole("dialog", { name: "Удалить исследование #17?" }));
  expect(requests.some((request) => request.method === "DELETE")).toBe(false);
  const confirm = confirmation.getByRole("button", { name: "Удалить" });
  fireEvent.click(confirm); fireEvent.click(confirm);
  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  await historyButton("Результат исследования 20");
  expect(screen.queryByRole("button", { name: "Результат исследования 17" })).not.toBeInTheDocument();
  expect(requests.filter((request) => request.method === "DELETE").map((request) => request.path)).toEqual(["/v1/optimization-studies/17"]);
  expect(requests.some((request) => request.method === "PUT")).toBe(false);
});

it("retains the history row and shows a deletion conflict without claiming success", async () => {
  studies = [record("completed")]; deleteStatus = 409;
  render(<AlgorithmsPage />);
  fireEvent.click(await historyButton("Удалить исследование 17"));
  const confirmation = within(await screen.findByRole("dialog", { name: "Удалить исследование #17?" }));
  fireEvent.click(confirmation.getByRole("button", { name: "Удалить" }));
  expect(await confirmation.findByRole("alert")).toHaveTextContent("Нельзя удалить активное исследование");
  fireEvent.click(confirmation.getByRole("button", { name: "Отмена" }));
  expect(await historyButton("Результат исследования 17")).toBeInTheDocument();
});

it.each(["completed", "cancelled", "failed"] as const)("allows explicit deletion of an archived continuous %s study without touching other rows or fixed drafts", async (status) => {
  studies = [{ ...record("completed"), id: 20, origin: "manual" },
    { ...record(status), origin: "continuous", continuousGenerationId: null, continuousSlotId: null }];
  render(<AlgorithmsPage />);
  fireEvent.click(await screen.findByRole("tab", { name: "Live-сканирование с постоянными параметрами" }));
  const fixed = await screen.findByRole("spinbutton", { name: "Новый порог (%)" });
  fireEvent.change(fixed, { target: { value: "8.5" } });
  fireEvent.click(screen.getByRole("tab", { name: "Live-сканирование с гибкими параметрами" }));
  await userEvent.click(await historyButton("Удалить исследование 17"));
  const confirmation = within(await screen.findByRole("dialog", { name: "Удалить исследование #17?" }));
  expect(requests.every((request) => request.method === "GET")).toBe(true);
  await userEvent.click(confirmation.getByRole("button", { name: "Отмена" }));
  expect(screen.getByRole("button", { name: "Результат исследования 17" })).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "Удалить исследование 17" }));
  await userEvent.click(within(await screen.findByRole("dialog", { name: "Удалить исследование #17?" })).getByRole("button", { name: "Удалить" }));
  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  await waitFor(() => expect(screen.queryByRole("button", { name: "Результат исследования 17" })).not.toBeInTheDocument());
  expect(screen.getByRole("button", { name: "Результат исследования 20" })).toBeInTheDocument();
  expect(fixed).toHaveValue(8.5);
  expect(requests.filter((request) => request.method !== "GET").map(({ path, method }) => ({ path, method }))).toEqual([
    { path: "/v1/optimization-studies/17", method: "DELETE" },
  ]);
});

it("explains study deletion authentication failure and retains the confirmation and row", async () => {
  studies = [record("failed")]; deleteStatus = 401;
  render(<AlgorithmsPage />);
  await userEvent.click(await historyButton("Удалить исследование 17"));
  const confirmation = within(await screen.findByRole("dialog", { name: "Удалить исследование #17?" }));
  await userEvent.click(confirmation.getByRole("button", { name: "Удалить" }));
  expect(await confirmation.findByRole("alert")).toHaveTextContent(/Сессия истекла|Войдите/);
  expect(confirmation.getByRole("button", { name: "Удалить" })).toBeEnabled();
  expect(studies).toHaveLength(1);
  expect(requests.filter((request) => request.method !== "GET")).toHaveLength(1);
});

it("distinguishes retained-chain deletion conflict and refreshes study discovery without auto-stopping", async () => {
  studies = [{ ...record("failed"), origin: "continuous", continuousGenerationId: 7, continuousSlotId: 5 }];
  deleteStatus = 409; deleteErrorCode = "continuous_history_referenced";
  render(<AlgorithmsPage />);
  await userEvent.click(await historyButton("Удалить исследование 17"));
  const confirmation = within(await screen.findByRole("dialog", { name: "Удалить исследование #17?" }));
  const readsBefore = requests.filter((request) => request.path.startsWith("/v1/optimization-studies?")).length;
  await userEvent.click(confirmation.getByRole("button", { name: "Удалить" }));
  expect(await confirmation.findByRole("alert")).toHaveTextContent(/связано с адаптивным профилем|непрерывной цепочкой/);
  await waitFor(() => expect(requests.filter((request) => request.path.startsWith("/v1/optimization-studies?")).length).toBeGreaterThan(readsBefore));
  expect(studies).toHaveLength(1);
  expect(requests.filter((request) => request.method !== "GET").map((request) => request.path)).toEqual(["/v1/optimization-studies/17"]);
});

it("previews owner-wide continuous cleanup once, cancels safely, and preserves drafts/manual rows after one confirmed command", async () => {
  studies = [{ ...record("completed"), id: 20, origin: "manual" }, { ...record("failed"), origin: "continuous" }];
  render(<AlgorithmsPage />);
  fireEvent.click(await screen.findByRole("tab", { name: "Live-сканирование с постоянными параметрами" }));
  const fixed = await screen.findByRole("spinbutton", { name: "Новый порог (%)" });
  fireEvent.change(fixed, { target: { value: "8.5" } });
  fireEvent.click(screen.getByRole("tab", { name: "Live-сканирование с гибкими параметрами" }));
  expect(screen.getAllByRole("button", { name: "Удалить историю непрерывных исследований…" })).toHaveLength(1);
  const controls = within(screen.getByRole("region", { name: "Непрерывные исследования" })).getByRole("region", { name: "Управление непрерывным исследованием" });
  expect(within(controls).getByRole("button", { name: "Удалить историю непрерывных исследований…" })).toHaveTextContent(/^$/);
  expect(requests.some((request) => request.path === "/v1/continuous-studies/cleanup")).toBe(false);
  await userEvent.click(screen.getByRole("button", { name: "Удалить историю непрерывных исследований…" }));
  let confirmation = within(await screen.findByRole("dialog", { name: "Удалить всю историю непрерывных исследований?" }));
  await confirmation.findByText("Непрерывные исследования: 12");
  expect(confirmation.getByText(/всех алгоритмов.*всё время/)).toBeInTheDocument();
  expect(confirmation.getByText(/Исторические прогоны: 9/)).toBeInTheDocument();
  expect(confirmation.getByText(/Аналитические результаты: 4/)).toBeInTheDocument();
  expect(confirmation.getByRole("button", { name: "Отмена" })).toHaveFocus();
  expect(requests.every((request) => request.method === "GET")).toBe(true);
  await userEvent.click(confirmation.getByRole("button", { name: "Отмена" }));
  expect(screen.getByRole("button", { name: "Удалить историю непрерывных исследований…" })).toHaveFocus();
  await userEvent.click(screen.getByRole("button", { name: "Удалить историю непрерывных исследований…" }));
  confirmation = within(await screen.findByRole("dialog", { name: "Удалить всю историю непрерывных исследований?" }));
  const confirm = await confirmation.findByRole("button", { name: "Удалить историю" });
  await waitFor(() => expect(confirm).toBeEnabled());
  fireEvent.click(confirm); fireEvent.click(confirm);
  await screen.findByText(/История непрерывных исследований удалена/);
  await waitFor(() => expect(screen.queryByRole("button", { name: "Результат исследования 17" })).not.toBeInTheDocument());
  expect(screen.getByRole("button", { name: "Результат исследования 20" })).toBeInTheDocument();
  expect(fixed).toHaveValue(8.5);
  expect(requests.filter((request) => request.method !== "GET").map(({ path, method, body }) => ({ path, method, body }))).toEqual([
    { path: "/v1/continuous-studies/cleanup", method: "POST", body: { confirm: true, fingerprint: cleanupFixture.fingerprint } },
  ]);
  expect(requests.filter((request) => request.path.startsWith("/v1/continuous-studies?")).length).toBeGreaterThan(2);
});

it.each(["continuous_not_stopped", "ownership_conflict"])("disables bulk cleanup for the server blocker %s, not for unrelated manual work", async (blocker) => {
  studies = [record("running")];
  cleanupPreview = { ...cleanupFixture, eligible: false, blockers: [blocker] };
  render(<AlgorithmsPage />);
  await userEvent.click(await screen.findByRole("button", { name: "Удалить историю непрерывных исследований…" }));
  const confirmation = within(await screen.findByRole("dialog", { name: "Удалить всю историю непрерывных исследований?" }));
  await confirmation.findByText("Непрерывные исследования: 12");
  expect(confirmation.getByRole("button", { name: "Удалить историю" })).toBeDisabled();
  expect(confirmation.getByRole("alert")).toHaveTextContent(blocker === "ownership_conflict" ? /владени/ : /остановите.*дождитесь/i);
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it("requires a new explicit confirmation with the replacement fingerprint after a stale cleanup preview", async () => {
  cleanupStatus = 409;
  render(<AlgorithmsPage />);
  await userEvent.click(await screen.findByRole("button", { name: "Удалить историю непрерывных исследований…" }));
  const confirmation = within(await screen.findByRole("dialog", { name: "Удалить всю историю непрерывных исследований?" }));
  await confirmation.findByText("Непрерывные исследования: 12");
  cleanupPreview = { ...cleanupFixture, fingerprint: "b".repeat(32), counts: { ...cleanupFixture.counts, studies: 13 } };
  await userEvent.click(confirmation.getByRole("button", { name: "Удалить историю" }));
  expect(await confirmation.findByRole("alert")).toHaveTextContent(/состав.*изменился|подтвердите заново/i);
  expect(confirmation.getByText("Непрерывные исследования: 13")).toBeInTheDocument();
  expect(requests.filter((request) => request.method === "POST")).toHaveLength(1);
  cleanupStatus = 200;
  await userEvent.click(confirmation.getByRole("button", { name: "Удалить историю" }));
  await screen.findByText(/История непрерывных исследований удалена/);
  expect(requests.filter((request) => request.method === "POST").at(-1)?.body).toEqual({ confirm: true, fingerprint: "b".repeat(32) });
});

it("keeps the exact selected manual detail open across continuous cleanup", async () => {
  studies = [{ ...record("completed"), id: 20, origin: "manual" }, { ...record("failed"), origin: "continuous" }];
  render(<AlgorithmsPage />);
  await userEvent.click(await historyButton("Результат исследования 20"));
  await screen.findByRole("dialog", { name: "Исследование #20 · Новый алгоритм" });
  // Simulates an owner-wide cleanup completing while an existing selection is retained.
  fireEvent.click(screen.getByRole("button", { name: "Удалить историю непрерывных исследований…", hidden: true }));
  const confirmation = within(await screen.findByRole("dialog", { name: "Удалить всю историю непрерывных исследований?" }));
  await confirmation.findByText("Непрерывные исследования: 12");
  await userEvent.click(confirmation.getByRole("button", { name: "Удалить историю" }));
  await screen.findByText(/История непрерывных исследований удалена/);
  expect(screen.getByRole("dialog", { name: "Исследование #20 · Новый алгоритм" })).toBeInTheDocument();
  expect(studies.map((study) => study.id)).toEqual([20]);
});

it("preserves the mounted manual form and its shared saved draft when cleanup refreshes controllers", async () => {
  studies = [{ ...record("failed"), origin: "continuous" }];
  render(<AlgorithmsPage />);
  const manual = within(await openOptimizer());
  const horizon = await manual.findByLabelText("Горизонт сделки, мин (фиксирован)");
  fireEvent.change(horizon, { target: { value: "720" } });
  fireEvent.blur(horizon);
  await waitFor(() => expect(drafts.future_algorithm).toMatchObject({ shared: { scope: { horizonMinutes: "720" } } }));
  const savedDraft = structuredClone(drafts.future_algorithm);
  fireEvent.click(screen.getByRole("button", { name: "Удалить историю непрерывных исследований…", hidden: true }));
  const confirmation = within(await screen.findByRole("dialog", { name: "Удалить всю историю непрерывных исследований?" }));
  await confirmation.findByText("Непрерывные исследования: 12");
  await userEvent.click(confirmation.getByRole("button", { name: "Удалить историю" }));
  await screen.findByText(/История непрерывных исследований удалена/);
  expect(manual.getByLabelText("Горизонт сделки, мин (фиксирован)")).toBe(horizon);
  expect(horizon).toHaveValue(720);
  expect(drafts.future_algorithm).toEqual(savedDraft);
  expect(requests.filter((request) => request.method === "POST").map((request) => request.path)).toEqual(["/v1/continuous-studies/cleanup"]);
});

it("closes a deleted continuous selection without closing retained manual optimizer state", async () => {
  studies = [{ ...record("completed"), id: 20, origin: "manual" }, { ...record("failed"), origin: "continuous" }];
  render(<AlgorithmsPage />);
  await userEvent.click(await historyButton("Результат исследования 17"));
  await screen.findByRole("dialog", { name: "Исследование #17 · Новый алгоритм" });
  fireEvent.click(screen.getByRole("button", { name: "Удалить историю непрерывных исследований…", hidden: true }));
  const confirmation = within(await screen.findByRole("dialog", { name: "Удалить всю историю непрерывных исследований?" }));
  await confirmation.findByText("Непрерывные исследования: 12");
  await userEvent.click(confirmation.getByRole("button", { name: "Удалить историю" }));
  await screen.findByText(/История непрерывных исследований удалена/);
  expect(screen.queryByRole("dialog", { name: "Исследование #17 · Новый алгоритм" })).not.toBeInTheDocument();
  expect(await historyButton("Результат исследования 20")).toBeInTheDocument();
});

it("blocks a new study for another algorithm while the platform already has active work", async () => {
  studies = [{ ...record(), spec: { ...spec, algorithmId: "second_algorithm" } }];
  render(<AlgorithmsPage />);
  const dialog = await openOptimizer();
  expect(await within(dialog).findByText(/В платформе допускается только одно активное исследование/)).toBeInTheDocument();
  expect(within(dialog).getByRole("button", { name: "Начать исследование" })).toBeDisabled();
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});
