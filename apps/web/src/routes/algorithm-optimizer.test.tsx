import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { summarizeOptimizationStudy, type OptimizationSpec, type OptimizationStudy } from "../lib/optimization-api";
import { formatUnix } from "../lib/datetime-format";
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
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

async function openOptimizer() {
  await userEvent.click(await screen.findByRole("button", { name: "Оптимизировать Новый алгоритм" }));
  return screen.findByRole("dialog", { name: "Оптимизация · Новый алгоритм" });
}
function enterDates() {
  fireEvent.change(screen.getByLabelText("Начало периода сигналов"), { target: { value: "2026-10-01T00:00" } });
  fireEvent.change(screen.getByLabelText("Конец периода сигналов"), { target: { value: "2026-10-03T00:00" } });
}

it("shows three separated sections and keeps fixed controls independent when the continuous backend is unavailable", async () => {
  render(<AlgorithmsPage />);
  const first = within((await screen.findByText("Новый алгоритм · 2.0")).closest("form")!);
  expect(first.getAllByRole("heading", { level: 4 }).map((heading) => heading.textContent)).toEqual([
    "Live-сканирование с постоянными параметрами", "Live-сканирование с гибкими параметрами", "Оптимизации",
  ]);
  expect(first.getAllByRole("separator")).toHaveLength(2);
  const constant = within(first.getByRole("region", { name: "Live-сканирование с постоянными параметрами" }));
  const flexible = within(first.getByRole("region", { name: "Live-сканирование с гибкими параметрами" }));
  expect(flexible.getByRole("checkbox", { name: "Разрешить адаптивное live-сканирование" })).toBeDisabled();
  expect(flexible.getByRole("button", { name: "Сохранить разрешение адаптивного live" })).toBeDisabled();
  expect(first.getByRole("button", { name: "Оптимизировать Новый алгоритм" })).toHaveTextContent("Оптимизировать");
  expect(first.getByRole("button", { name: "Посмотреть весь список" })).toHaveAttribute("type", "button");
  expect(screen.queryByText("Статус / результат оптимизации")).not.toBeInTheDocument();
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
  expect(screen.getAllByRole("tabpanel")).toHaveLength(1);
  expect(firstTab).toHaveAccessibleDescription("Live-сканирование включено");
  expect(secondTab.querySelector('[data-slot="sidebar-status-dot"]')).toBeNull();
  const firstInput = screen.getByRole("spinbutton", { name: "Новый порог (%)" });
  fireEvent.change(firstInput, { target: { value: "8.5" } });
  await userEvent.click(screen.getByRole("checkbox", { name: "Включён для live-сканирования" }));
  expect(firstTab.querySelector('[data-slot="sidebar-status-dot"]')).toBeInTheDocument();
  await userEvent.click(secondTab);
  expect(firstInput).toBeInTheDocument(); expect(firstInput).not.toBeVisible();
  const secondInput = screen.getByRole("spinbutton", { name: "Новый порог (%)" });
  expect(secondInput).not.toBe(firstInput); expect(secondInput).toHaveValue(2.5);
  fireEvent.change(secondInput, { target: { value: "9.5" } });
  await userEvent.click(firstTab);
  expect(screen.getByRole("spinbutton", { name: "Новый порог (%)" })).toBe(firstInput);
  expect(firstInput).toHaveValue(8.5);
  expect(screen.getByRole("checkbox", { name: "Включён для live-сканирования" })).not.toBeChecked();
  expect(screen.getAllByRole("tabpanel")).toHaveLength(1);
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
  expect(creates[0]?.body).toEqual({
    ...spec, fromUnix: new Date(2026, 9, 1).getTime() / 1000, toUnix: new Date(2026, 9, 3).getTime() / 1000,
    direction: "long", tpLevels: [4, 25], slLevels: [0.5, 6], minSetupsPerDay: 4,
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
  fireEvent.click(await screen.findByRole("button", { name: `Оптимизировать ${other.name}` }));
  const previous = within(await screen.findByRole("dialog", { name: `Оптимизация · ${other.name}` }));
  fireEvent.change(await previous.findByLabelText(`Baseline: ${otherLabel}`), { target: { value: "91" } });
  await userEvent.click(previous.getByRole("button", { name: "Закрыть" }));
  await userEvent.click(screen.getByRole("tab", { name: target.name }));
  fireEvent.click(screen.getByRole("button", { name: `Оптимизировать ${target.name}` }));
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
  await userEvent.click(screen.getByRole("button", { name: "Оптимизировать Второй алгоритм" }));
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
  await waitFor(() => expect(screen.getByRole("button", { name: "Оптимизировать Новый алгоритм" })).toHaveFocus());
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
  studies = [{ ...record("completed"), result: { progress: { ...progress, bestScore: best.score }, best, reason: "trial_budget" } }];
  render(<AlgorithmsPage />);
  await openOptimizer();
  expect(await screen.findByText("Лучший score: 0.125 R")).toBeInTheDocument();
  expect(screen.getByText("snapshot-test-hash")).toBeInTheDocument();
  expect(screen.getByRole("table", { name: "Матрица и поддержка результата" })).toHaveTextContent("Закрытых");
  expect(screen.getByText(/не доказанный глобальный оптимум/)).toBeInTheDocument();
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
  expect(lists.filter((request) => new URL(request.path, "http://localhost").searchParams.get("limit") === "20")).toHaveLength(2);
  expect(lists.filter((request) => new URL(request.path, "http://localhost").searchParams.get("limit") === "5")).toHaveLength(2);
  expect(lists.filter((request) => new URL(request.path, "http://localhost").searchParams.get("limit") === "10")).toHaveLength(0);
  expect(requests.filter((request) => request.path === "/v1/optimization-studies/17")).toHaveLength(0);
  fireEvent.click(screen.getByRole("button", { name: "Оптимизировать Новый алгоритм" }));
  await act(async () => { await vi.advanceTimersByTimeAsync(0); });
  expect(requests.filter((request) => request.path === "/v1/optimization-studies/17")).toHaveLength(1);
  await act(async () => { await vi.advanceTimersByTimeAsync(9000); });
  expect(requests.filter((request) => request.path === "/v1/optimization-studies/17")).toHaveLength(1);
  studies = [{ ...studies[0]!, updatedAt: "2026-11-10T00:02:00Z" }];
  await act(async () => { await vi.advanceTimersByTimeAsync(3000); });
  expect(requests.filter((request) => request.path === "/v1/optimization-studies/17")).toHaveLength(2);
  fireEvent.click(screen.getByRole("button", { name: "Закрыть" }));
  await act(async () => { await vi.advanceTimersByTimeAsync(9000); });
  expect(requests.filter((request) => request.path === "/v1/optimization-studies/17")).toHaveLength(2);
});

it("shows five inline summaries without pagination and all history in an open paginated dialog", async () => {
  studies = Array.from({ length: 12 }, (_, index) => ({ ...record("completed"), id: 30 - index,
    progress: { ...progress, elapsedSeconds: index === 1 ? 90061 : 12, bestScore: index === 0 ? 0 : index === 1 ? null : index === 3 ? -0.126 : index === 4 ? 1e21 : 0.25 },
    spec: { ...spec, ...(index === 0 ? { minSetupsPerDay: 0 } : index > 1 ? { minSetupsPerDay: 4 } : {}) },
  }));
  render(<AlgorithmsPage />);
  const table = within(await screen.findByRole("table", { name: "Исследования · Новый алгоритм" }));
  await table.findByRole("button", { name: "Результат исследования 30" });
  expect(table.getAllByRole("columnheader").map((header) => header.textContent)).toEqual(["ID", "Происхождение", "Диапазон дат", "Время исследования", "Минимум сетапов в день", "Лучшее среднее матожидание", "Действия"]);
  expect(table.getAllByRole("row")).toHaveLength(6);
  const first = within(table.getAllByRole("row")[1]!);
  expect(first.getAllByRole("cell").map((cell) => cell.textContent)).toEqual(["#30", "Ручное", `[${formatUnix(spec.fromUnix)}, ${formatUnix(spec.toUnix)})`, "Менее 1 мин", "0", "0.00 R", ""]);
  expect(table.getAllByRole("row")[2]).toHaveTextContent("Гейт не применялся");
  expect(table.getAllByRole("row")[2]).toHaveTextContent("—");
  expect(table.getAllByRole("row")[2]).toHaveTextContent("1 дн 1 ч 1 мин");
  expect(table.getAllByRole("row")[3]).toHaveTextContent("0.25 R");
  expect(table.getAllByRole("row")[4]).toHaveTextContent("-0.13 R");
  expect(table.getAllByRole("row")[5]).toHaveTextContent("1000000000000000000000.00 R");
  const section = within(screen.getByRole("region", { name: "История оптимизации · Новый алгоритм" }));
  expect(section.queryByRole("button", { name: /страница исследований/ })).not.toBeInTheDocument();
  expect(requests.some((request) => new URL(request.path, "http://localhost").searchParams.get("limit") === "10")).toBe(false);
  const inlineTrigger = table.getByRole("button", { name: "Результат исследования 30" });
  const card = within(screen.getByText("Новый алгоритм · 2.0").closest("form")!);
  await userEvent.click(card.getByRole("button", { name: "Посмотреть весь список" }));
  const full = within(await screen.findByRole("dialog", { name: "Все оптимизации · Новый алгоритм" }));
  const fullTable = within(full.getByRole("table", { name: "Исследования · Новый алгоритм" }));
  const fullTrigger = await fullTable.findByRole("button", { name: "Результат исследования 30" });
  expect(fullTrigger.id).not.toBe(inlineTrigger.id);
  const ids = [...document.querySelectorAll("[id]")].map((element) => element.id);
  expect(new Set(ids).size).toBe(ids.length);
  expect(fullTable.getAllByRole("row")).toHaveLength(11);
  expect(fullTable.getAllByRole("columnheader").map((header) => header.textContent)).toEqual(table.getAllByRole("columnheader", { hidden: true }).map((header) => header.textContent));
  expect(fullTable.getAllByRole("row")[1]).toHaveTextContent("0.00 R");
  expect(fullTable.getAllByRole("row")[2]).toHaveTextContent("1 дн 1 ч 1 мин");
  expect(fullTable.getAllByRole("row")[4]).toHaveTextContent("-0.13 R");
  expect(fullTable.getAllByRole("row")[5]).toHaveTextContent("1000000000000000000000.00 R");
  fireEvent.click(full.getByRole("button", { name: /Следующая страница/ }));
  await fullTable.findByRole("button", { name: "Результат исследования 20" });
  expect(fullTable.getAllByRole("row")).toHaveLength(3);
  expect(full.getByRole("button", { name: /Следующая страница/ })).toBeDisabled();
  expect(requests.some((request) => new URL(request.path, "http://localhost").searchParams.get("offset") === "10")).toBe(true);
  expect(requests.some((request) => /^\/v1\/optimization-studies\/\d+$/.test(request.path))).toBe(false);
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it("keeps the full list underneath exact-ID detail and restores its page and unique trigger focus", async () => {
  studies = Array.from({ length: 12 }, (_, index) => ({ ...record("completed"), id: 30 - index }));
  render(<AlgorithmsPage />);
  const card = within((await screen.findByText("Новый алгоритм · 2.0")).closest("form")!);
  const listTrigger = card.getByRole("button", { name: "Посмотреть весь список" });
  await userEvent.click(listTrigger);
  const listDialog = await screen.findByRole("dialog", { name: "Все оптимизации · Новый алгоритм" });
  const list = within(listDialog);
  await list.findByRole("button", { name: "Результат исследования 30" });
  await userEvent.click(list.getByRole("button", { name: /Следующая страница/ }));
  const idTrigger = await list.findByRole("button", { name: "Результат исследования 19" });
  await userEvent.click(idTrigger);
  const detail = within(await screen.findByRole("dialog", { name: "Исследование #19 · Новый алгоритм" }));
  expect(await detail.findByText("Исследование #19")).toBeInTheDocument();
  expect(listDialog).toBeInTheDocument();
  expect(listDialog).toHaveAttribute("data-state", "open");
  expect(document.querySelectorAll('[data-slot="dialog-content"]')).toHaveLength(2);
  await userEvent.click(detail.getByRole("button", { name: "Закрыть" }));
  expect(await screen.findByRole("dialog", { name: "Все оптимизации · Новый алгоритм" })).toBe(listDialog);
  await waitFor(() => expect(list.getByRole("button", { name: "Результат исследования 19" })).toHaveFocus());
  expect(list.getByText("Страница 2 · всего 12")).toBeInTheDocument();
  expect(requests.filter((request) => /^\/v1\/optimization-studies\/\d+$/.test(request.path)).map((request) => request.path)).toEqual(["/v1/optimization-studies/19"]);
  await userEvent.click(list.getByRole("button", { name: "Закрыть" }));
  await waitFor(() => expect(listTrigger).toHaveFocus());
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it("does not fetch the full history while closed, but continues compact active observation", async () => {
  vi.useFakeTimers(); studies = [record()];
  render(<AlgorithmsPage />);
  await act(async () => { await vi.advanceTimersByTimeAsync(0); });
  const fullRequests = () => requests.filter((request) => new URL(request.path, "http://localhost").searchParams.get("limit") === "10").length;
  expect(fullRequests()).toBe(0);
  const card = within(screen.getByText("Новый алгоритм · 2.0").closest("form")!);
  fireEvent.click(card.getByRole("button", { name: "Посмотреть весь список" }));
  await act(async () => { await vi.advanceTimersByTimeAsync(0); });
  expect(fullRequests()).toBe(1);
  fireEvent.click(screen.getByRole("button", { name: "Закрыть" }));
  const inlineBefore = requests.filter((request) => new URL(request.path, "http://localhost").searchParams.get("limit") === "5").length;
  await act(async () => { await vi.advanceTimersByTimeAsync(9000); });
  expect(fullRequests()).toBe(1);
  expect(requests.filter((request) => new URL(request.path, "http://localhost").searchParams.get("limit") === "5").length).toBeGreaterThan(inlineBefore);
  expect(requests.some((request) => /^\/v1\/optimization-studies\/\d+$/.test(request.path))).toBe(false);
});

it("corrects full-list pagination after deletion and restores the underlying list after detail closes", async () => {
  studies = Array.from({ length: 11 }, (_, index) => ({ ...record("completed"), id: 30 - index }));
  render(<AlgorithmsPage />);
  const card = within((await screen.findByText("Новый алгоритм · 2.0")).closest("form")!);
  await userEvent.click(card.getByRole("button", { name: "Посмотреть весь список" }));
  const listDialog = await screen.findByRole("dialog", { name: "Все оптимизации · Новый алгоритм" });
  const list = within(listDialog);
  await list.findByRole("button", { name: "Результат исследования 30" });
  await userEvent.click(list.getByRole("button", { name: /Следующая страница/ }));
  await userEvent.click(await list.findByRole("button", { name: "Результат исследования 20" }));
  await screen.findByText("Исследование #20");
  await userEvent.click(screen.getByRole("button", { name: "Удалить исследование 20" }));
  const confirmation = within(await screen.findByRole("dialog", { name: "Удалить исследование #20?" }));
  await userEvent.click(confirmation.getByRole("button", { name: "Удалить" }));
  expect(await screen.findByRole("dialog", { name: "Все оптимизации · Новый алгоритм" })).toBe(listDialog);
  await list.findByText("Страница 1 · всего 10");
  expect(list.queryByRole("button", { name: "Результат исследования 20" })).not.toBeInTheDocument();
  expect(list.getByRole("button", { name: /Предыдущая страница/ })).toBeDisabled();
  expect(list.getByRole("button", { name: /Следующая страница/ })).toBeDisabled();
  expect(requests.filter((request) => request.method === "DELETE").map((request) => request.path)).toEqual(["/v1/optimization-studies/20"]);
});

it("preserves stop and pending actions in the full-history dialog", async () => {
  studies = [record()];
  render(<AlgorithmsPage />);
  const card = within((await screen.findByText("Новый алгоритм · 2.0")).closest("form")!);
  await userEvent.click(card.getByRole("button", { name: "Посмотреть весь список" }));
  const list = within(await screen.findByRole("dialog", { name: "Все оптимизации · Новый алгоритм" }));
  const stop = await list.findByRole("button", { name: "Остановить исследование 17" });
  fireEvent.click(stop); fireEvent.click(stop);
  await list.findByText("Остановка запрошена");
  expect(list.getByRole("button", { name: "Остановить исследование 17" })).toBeDisabled();
  expect(requests.filter((request) => request.path.endsWith("/cancel"))).toHaveLength(1);
  expect(requests.some((request) => request.method === "PUT")).toBe(false);
});

it("shows a stop mutation error inside the full-history dialog", async () => {
  studies = [record()]; cancelStatus = 503;
  render(<AlgorithmsPage />);
  const card = within((await screen.findByText("Новый алгоритм · 2.0")).closest("form")!);
  await userEvent.click(card.getByRole("button", { name: "Посмотреть весь список" }));
  const list = within(await screen.findByRole("dialog", { name: "Все оптимизации · Новый алгоритм" }));
  await userEvent.click(await list.findByRole("button", { name: "Остановить исследование 17" }));
  expect(await list.findByRole("alert")).toHaveTextContent("HTTP 503");
  expect(list.getByRole("button", { name: "Остановить исследование 17" })).toBeEnabled();
  expect(requests.filter((request) => request.path.endsWith("/cancel"))).toHaveLength(1);
});

it("opens the requested older ID independently from latest discovery without nested optimizer dialogs", async () => {
  studies = [{ ...record("completed"), id: 20 }, { ...record("cancelled"), id: 17 }];
  render(<AlgorithmsPage />);
  fireEvent.click(await screen.findByRole("button", { name: "Результат исследования 17" }));
  expect(await screen.findByText("Исследование #17")).toBeInTheDocument();
  expect(screen.getAllByRole("dialog")).toHaveLength(1);
  studies.unshift({ ...record("completed"), id: 21 });
  fireEvent.click(screen.getByRole("button", { name: "Обновить статус" }));
  await waitFor(() => expect(requests.some((request) => request.path.startsWith("/v1/optimization-studies?"))).toBe(true));
  expect(screen.getByText("Исследование #17")).toBeInTheDocument();
  expect(requests.some((request) => request.path === "/v1/optimization-studies/20" || request.path === "/v1/optimization-studies/21")).toBe(false);
  await userEvent.click(screen.getByRole("button", { name: "Закрыть" }));
  fireEvent.click(await screen.findByRole("button", { name: "Результат исследования 17" }));
  expect(await screen.findByText("Исследование #17")).toBeInTheDocument();
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it.each(["queued", "running"] as const)("stops an active %s history row once without submitting settings", async (status) => {
  studies = [record(status)];
  render(<AlgorithmsPage />);
  const stop = await screen.findByRole("button", { name: "Остановить исследование 17" });
  fireEvent.click(stop); fireEvent.click(stop);
  await waitFor(() => expect(screen.getByRole("button", { name: "Остановить исследование 17" })).toBeDisabled());
  expect(await screen.findByText("Остановка запрошена")).toBeInTheDocument();
  expect(requests.filter((request) => request.path.endsWith("/cancel"))).toHaveLength(1);
  expect(requests.some((request) => request.method === "PUT")).toBe(false);
});

it("does not offer repeated stop or deletion for cancel_requested", async () => {
  studies = [record("cancel_requested")];
  render(<AlgorithmsPage />);
  const stop = await screen.findByRole("button", { name: "Остановить исследование 17" });
  expect(stop).toBeDisabled(); fireEvent.click(stop);
  expect(screen.queryByRole("button", { name: "Удалить исследование 17" })).not.toBeInTheDocument();
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it.each(["completed", "cancelled", "failed"] as const)("confirms terminal %s deletion, closes selected detail and refreshes history", async (status) => {
  studies = [{ ...record("completed"), id: 20 }, record(status)];
  render(<AlgorithmsPage />);
  fireEvent.click(await screen.findByRole("button", { name: "Результат исследования 17" }));
  await screen.findByText("Исследование #17");
  fireEvent.click(screen.getByRole("button", { name: "Удалить исследование 17" }));
  const confirmation = within(await screen.findByRole("dialog", { name: "Удалить исследование #17?" }));
  expect(requests.some((request) => request.method === "DELETE")).toBe(false);
  const confirm = confirmation.getByRole("button", { name: "Удалить" });
  fireEvent.click(confirm); fireEvent.click(confirm);
  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  await screen.findByRole("button", { name: "Результат исследования 20" });
  expect(screen.queryByRole("button", { name: "Результат исследования 17" })).not.toBeInTheDocument();
  expect(requests.filter((request) => request.method === "DELETE").map((request) => request.path)).toEqual(["/v1/optimization-studies/17"]);
  expect(requests.some((request) => request.method === "PUT")).toBe(false);
});

it("retains the history row and shows a deletion conflict without claiming success", async () => {
  studies = [record("completed")]; deleteStatus = 409;
  render(<AlgorithmsPage />);
  fireEvent.click(await screen.findByRole("button", { name: "Удалить исследование 17" }));
  const confirmation = within(await screen.findByRole("dialog", { name: "Удалить исследование #17?" }));
  fireEvent.click(confirmation.getByRole("button", { name: "Удалить" }));
  expect(await confirmation.findByRole("alert")).toHaveTextContent("Нельзя удалить активное исследование");
  fireEvent.click(confirmation.getByRole("button", { name: "Отмена" }));
  expect(await screen.findByRole("button", { name: "Результат исследования 17" })).toBeInTheDocument();
});

it.each(["completed", "cancelled", "failed"] as const)("allows explicit deletion of an archived continuous %s study without touching other rows or fixed drafts", async (status) => {
  studies = [{ ...record("completed"), id: 20, origin: "manual" },
    { ...record(status), origin: "continuous", continuousGenerationId: null, continuousSlotId: null }];
  render(<AlgorithmsPage />);
  const fixed = await screen.findByRole("spinbutton", { name: "Новый порог (%)" });
  fireEvent.change(fixed, { target: { value: "8.5" } });
  await userEvent.click(await screen.findByRole("button", { name: "Удалить исследование 17" }));
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
  await userEvent.click(await screen.findByRole("button", { name: "Удалить исследование 17" }));
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
  await userEvent.click(await screen.findByRole("button", { name: "Удалить исследование 17" }));
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
  const fixed = await screen.findByRole("spinbutton", { name: "Новый порог (%)" });
  fireEvent.change(fixed, { target: { value: "8.5" } });
  expect(screen.getAllByRole("button", { name: "Удалить историю непрерывных исследований…" })).toHaveLength(1);
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
  await userEvent.click(await screen.findByRole("button", { name: "Результат исследования 20" }));
  await screen.findByRole("dialog", { name: "Исследование #20 · Новый алгоритм" });
  // Simulates an owner-wide cleanup completing while an existing selection is retained.
  fireEvent.click(screen.getByText("Удалить историю непрерывных исследований…"));
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
  fireEvent.click(screen.getByText("Удалить историю непрерывных исследований…"));
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
  await userEvent.click(await screen.findByRole("button", { name: "Результат исследования 17" }));
  await screen.findByRole("dialog", { name: "Исследование #17 · Новый алгоритм" });
  fireEvent.click(screen.getByText("Удалить историю непрерывных исследований…"));
  const confirmation = within(await screen.findByRole("dialog", { name: "Удалить всю историю непрерывных исследований?" }));
  await confirmation.findByText("Непрерывные исследования: 12");
  await userEvent.click(confirmation.getByRole("button", { name: "Удалить историю" }));
  await screen.findByText(/История непрерывных исследований удалена/);
  expect(screen.queryByRole("dialog", { name: "Исследование #17 · Новый алгоритм" })).not.toBeInTheDocument();
  expect(await screen.findByRole("button", { name: "Результат исследования 20" })).toBeInTheDocument();
});

it("blocks a new study for another algorithm while the platform already has active work", async () => {
  studies = [{ ...record(), spec: { ...spec, algorithmId: "second_algorithm" } }];
  render(<AlgorithmsPage />);
  const dialog = await openOptimizer();
  expect(await within(dialog).findByText(/В платформе допускается только одно активное исследование/)).toBeInTheDocument();
  expect(within(dialog).getByRole("button", { name: "Начать исследование" })).toBeDisabled();
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});
