import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { ContinuousConfig, ContinuousProcess, ContinuousSlot } from "../lib/continuous-api";
import { applyInterfaceSettings, formatUnix } from "../lib/datetime-format";
import { ContinuousResearch } from "./continuous-study";
import type { ContinuousController } from "./use-continuous-study";
import { summarizeOptimizationStudy, type OptimizationSpec, type OptimizationStudy } from "../lib/optimization-api";
import { AlgorithmsPage } from "./algorithms-page";
import type { Algorithm } from "./platform-controls";

const algorithm: Algorithm = { id: "future", name: "Stable service name", externalName: "Мой алгоритм", version: "1", description: "Metadata", enabled: false,
  params: { futureThreshold: "2.5", opaqueFixed: "kept" }, fields: [{ key: "futureThreshold", label: "Новый порог", type: "decimal", min: 0, max: 100, unit: "%", optimization: { enabledByDefault: true, values: ["1.5", "2.5"] } }] };
const spec: OptimizationSpec = { algorithmId: "future", algorithmVersion: "1", algorithmParams: algorithm.params, algorithmSearch: {}, analyticsParams: { clusterEnabled: true, clusterGapMinutes: 480, timeShiftMinutes: 3, priceChangePct: 1 }, analyticsSearch: {}, instruments: [{ exchange: "bybit", category: "linear", symbol: "OLDUSDT" }], fromUnix: 1793491200, toUnix: 1794096000, direction: "short", setupMaturity: 0, horizonMinutes: 10080, tpLevels: [4], slLevels: [2], minClosedPerCell: 1, seed: 42, maxTrials: 256, maxSeconds: 3600 };
function study(id: number, origin: "manual" | "continuous", status: OptimizationStudy["status"]): OptimizationStudy {
  return { id, origin, status, spec, progress: { phase: "acquiring", evaluatedCandidates: 0, scorableCandidates: 0, maxTrials: 256, elapsedSeconds: 1, bestScore: null }, result: null, error: null, createdAt: "2026-11-12T12:00:00Z", updatedAt: "2026-11-12T12:00:00Z", events: [] };
}
let process: ContinuousProcess | null;
let studies: OptimizationStudy[];
let unavailable: boolean;
let draftStatus: number;
let drafts: Record<string, unknown>;
const requests: { path: string; method: string; body: unknown }[] = [];
beforeEach(() => {
  process = null; studies = []; unavailable = false; draftStatus = 200; drafts = {}; requests.length = 0;
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    const body: unknown = typeof init?.body === "string" ? JSON.parse(init.body) : null;
    requests.push({ path, method, body });
    const json = (data: unknown, status = 200) => Promise.resolve(new Response(JSON.stringify(data), { status }));
    if (path === "/v1/algorithms") return json({ algorithms: [algorithm] });
    if (path === "/v1/instruments") return json({ instruments: [{ id: 1, exchange: "bybit", category: "linear", symbol: "OLDUSDT", status: "Delisted" }] });
    if (path === "/v1/settings/ui") return json({ value: { replay: { symbols: "OLDUSDT" } } });
    const draft = /^\/v1\/optimization-drafts\/(.+)$/.exec(path);
    if (draft) {
      const id = decodeURIComponent(draft[1]!);
      if (method === "PUT") {
        if (draftStatus !== 200) return json({ error: "draft_failed" }, draftStatus);
        drafts[id] = (body as { value: unknown }).value;
      }
      return json({ algorithmId: id, value: drafts[id] ?? null });
    }
    if (path.startsWith("/v1/continuous-studies?")) return unavailable ? json({}, 404) : json({ processes: process ? [process] : [] });
    if (path === "/v1/continuous-studies/future" && method === "PUT") {
      const config = body as ContinuousConfig;
      if (process?.config && JSON.stringify({ ...process.config, adaptiveEnabled: false }) === JSON.stringify({ ...config, adaptiveEnabled: false })) {
        process = { ...process, config: { ...process.config, adaptiveEnabled: config.adaptiveEnabled } };
        return json(process);
      }
      if (process && process.status !== "stopped") return json({ error: "continuous_process_active" }, 409);
      process = { ownerId: "admin", algorithmId: "future", status: "stopped", configRevision: (process?.configRevision ?? 0) + 1, config, activeStudyId: null, phase: null, error: null,
        slots: config.minSetupsPerDay.map((threshold, index) => ({ id: index + 1, minSetupsPerDay: threshold, status: "idle", activeGenerationId: null, studyId: null, meanExpectancy: null, selectedCell: null, updatedAt: "2026-11-12T12:00:00Z" })) };
      return json(process);
    }
    if (path.endsWith("/future/start") && process) { process = { ...process, status: "running", phase: "queued" }; return json(process, 202); }
    if (path.endsWith("/future/stop") && process) { process = { ...process, status: "stopping", phase: "cleanup", slots: process.slots.map((slot) => ({ ...slot, activeGenerationId: null, status: "stopped" })) }; return json(process, 202); }
    if (path.startsWith("/v1/optimization-studies?")) {
      const query = new URL(path, "http://localhost").searchParams;
      const limit = Number(query.get("limit")); const offset = Number(query.get("offset"));
      return json({ studies: studies.slice(offset, offset + limit).map(summarizeOptimizationStudy), total: studies.length, limit, offset });
    }
    const detail = /^\/v1\/optimization-studies\/(\d+)$/.exec(path);
    if (detail) return json(studies.find((item) => item.id === Number(detail[1])));
    if (path.startsWith("/v1/algorithms/") && method === "PUT") return json({});
    return json({});
  }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); applyInterfaceSettings({ timezone: "auto" }); });

async function configure() {
  const button = await screen.findByRole("button", { name: "Настроить непрерывное исследование" });
  await waitFor(() => expect(button).toBeEnabled());
  await userEvent.click(button);
  const dialog = within(await screen.findByRole("dialog", { name: "Непрерывное исследование · Мой алгоритм" }));
  await waitFor(() => expect(dialog.getByRole("button", { name: "Сохранить непрерывное исследование" })).toBeEnabled());
  return dialog;
}

const closedBoundary = Date.parse("2026-11-12T12:00:00Z") / 1000;
function profileView(patch: Partial<ContinuousSlot> = {}) {
  const controller: ContinuousController = {
    process: { ownerId: "admin", algorithmId: "future", status: "running", configRevision: 1,
      config: { lookbackDays: 30, minSetupsPerDay: [4], minMeanExpectancy: 0.15, adaptiveEnabled: true, study: spec }, activeStudyId: null, phase: "queued", error: null,
      slots: [{ id: 1, minSetupsPerDay: 4, status: "warming", activeGenerationId: 7, studyId: 42, meanExpectancy: 0.21,
        selectedCell: { tpPct: 14, slPct: 4, rawR: 0.32, meanR: 0.21, robustScore: 0.01 }, updatedAt: "2026-11-12T12:00:00Z", ...patch }] },
    available: true, loading: false, error: null, busy: null, revision: 0,
    refresh: vi.fn(), save: vi.fn(() => Promise.resolve(true)), start: vi.fn(() => Promise.resolve(true)), stop: vi.fn(() => Promise.resolve(true)),
  };
  return <ContinuousResearch algorithm={algorithm} controller={controller} />;
}
it("labels configured profiles separately from research history and marks cleared results without deleting the slot", () => {
  render(profileView({ status: "stopped", activeGenerationId: null, activeStatus: "inactive", studyId: null, meanExpectancy: null, selectedCell: null,
    pendingGenerationId: null, pendingStatus: null, readinessFrontier: null, pendingError: null, liveWatermark: null }));
  expect(screen.getByRole("heading", { name: "Настроенные адаптивные профили" })).toBeInTheDocument();
  const table = within(screen.getByRole("table", { name: "Настроенные адаптивные профили · Мой алгоритм" }));
  expect(table.getAllByRole("row")).toHaveLength(2);
  expect(table.getByText("Результатов нет")).toBeInTheDocument();
  expect(table.getByRole("columnheader", { name: "Изменение состояния профиля" })).toBeInTheDocument();
  expect(screen.getByText(/не дата исследования/)).toBeInTheDocument();
  expect(requests).toHaveLength(0);
});
it("separates successful research, incumbent live and the warming candidate without relabelling its study as the incumbent", () => {
  render(profileView({ activeStatus: "active", pendingGenerationId: 8, pendingStatus: "warming", readinessFrontier: null, pendingError: null, liveWatermark: closedBoundary }));
  const table = within(screen.getByRole("table", { name: "Настроенные адаптивные профили · Мой алгоритм" }));
  const incumbent = table.getByRole("cell", { name: /Действующее поколение #7/ });
  const candidate = table.getByRole("cell", { name: /Кандидат #8/ });
  expect(table.getByText("Исследование успешно · допуск пройден")).toBeInTheDocument();
  expect(table.getByText("Сохранённое исследование #42")).toBeInTheDocument();
  expect(incumbent).not.toHaveTextContent("#8"); expect(incumbent).not.toHaveTextContent("#42");
  expect(candidate).toHaveTextContent("Прогрев / догоняющая обработка");
  expect(candidate).not.toHaveTextContent("Готов к переключению");
  expect(table.getByText("SHORT · TP 14% / SL 4% · 0.32 R")).toBeInTheDocument();
  expect(requests).toHaveLength(0);
});
it("distinguishes ready-to-switch from active and shows exact CLOSED frontier/watermark without adding a minute", () => {
  applyInterfaceSettings({ timezone: "UTC" });
  const view = render(profileView({ activeStatus: "active", pendingGenerationId: 8, pendingStatus: "ready", readinessFrontier: closedBoundary, pendingError: null, liveWatermark: 0 }));
  const candidate = screen.getByRole("cell", { name: /Кандидат #8/ });
  expect(candidate).toHaveTextContent("Готов к переключению · ещё не действует");
  expect(candidate).toHaveTextContent(`Граница готовности (закрытие минуты): ${formatUnix(closedBoundary)} (UTC)`);
  expect(screen.getByRole("cell", { name: /Действующее поколение #7/ })).toHaveTextContent(`Watermark переключения: ${formatUnix(0)} (UTC)`);
  view.rerender(profileView({ status: "active", activeStatus: "active", activeGenerationId: 8, pendingGenerationId: null, pendingStatus: null, readinessFrontier: null, pendingError: null, liveWatermark: closedBoundary }));
  expect(screen.getByRole("cell", { name: /Действующее поколение #8/ })).toHaveTextContent(`Watermark переключения: ${formatUnix(closedBoundary)} (UTC)`);
  expect(screen.queryByText(/Действующее поколение #7/)).not.toBeInTheDocument();
  expect(screen.getByText("Нет кандидата на замену")).toBeInTheDocument();
});
it("keeps the incumbent during a candidate error and renders only bounded public error text", () => {
  render(profileView({ activeStatus: "active", pendingGenerationId: 8, pendingStatus: "error", readinessFrontier: null,
    pendingError: { code: "history_missing", message: "Недостаточно истории. " + "x".repeat(1000), debug: "do-not-render-raw-payload" }, liveWatermark: closedBoundary }));
  expect(screen.getByRole("cell", { name: /Действующее поколение #7/ })).toBeInTheDocument();
  expect(screen.getByRole("cell", { name: /Кандидат #8/ })).toHaveTextContent("Ошибка прогрева");
  const error = screen.getByRole("alert");
  expect(error).toHaveTextContent("history_missing"); expect(error).toHaveTextContent("Недостаточно истории.");
  expect(error.textContent.length).toBeLessThan(420); expect(error).not.toHaveTextContent("do-not-render-raw-payload");
  expect(screen.getByText("SHORT · TP 14% / SL 4% · 0.32 R")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Остановить непрерывное исследование" })).toBeEnabled();
  expect(requests).toHaveLength(0);
});
it("does not turn a first successful candidate into an active generation before promotion", () => {
  render(profileView({ activeGenerationId: null, activeStatus: "inactive", pendingGenerationId: 8, pendingStatus: "warming", readinessFrontier: null, pendingError: null, liveWatermark: null }));
  expect(screen.getByText("Нет действующего поколения")).toBeInTheDocument();
  expect(screen.getByRole("cell", { name: /Кандидат #8/ })).toHaveTextContent("Прогрев / догоняющая обработка");
  expect(screen.queryByText(/Действующее поколение #8/)).not.toBeInTheDocument();
  expect(screen.queryByText("SHORT · TP 14% / SL 4% · 0.32 R")).not.toBeInTheDocument();
});
it("labels legacy and partially rolled-out readiness as unknown rather than ready", () => {
  const view = render(profileView({ status: "active" }));
  expect(screen.getByText("Принятое поколение #7")).toBeInTheDocument();
  expect(screen.getByText("Состояние live неизвестно (старый API)")).toBeInTheDocument();
  expect(screen.getByText("Статус прогрева неизвестен (старый API)")).toBeInTheDocument();
  expect(screen.queryByText(/Действующее поколение #7/)).not.toBeInTheDocument();
  expect(screen.queryByText(/Готов к переключению/)).not.toBeInTheDocument();
  view.rerender(profileView({ pendingGenerationId: 8 }));
  expect(screen.getByRole("cell", { name: /Кандидат #8/ })).toHaveTextContent("Статус прогрева неизвестен");
});
it("shows an unrecognised candidate wait state without treating it as ready or dropping the incumbent", () => {
  render(profileView({ activeStatus: "active", pendingGenerationId: 8, pendingStatus: "waiting_resource", pendingError: "Ожидание ресурсного допуска" }));
  expect(screen.getByRole("cell", { name: /Кандидат #8/ })).toHaveTextContent("waiting_resource");
  expect(screen.queryByText(/Готов к переключению/)).not.toBeInTheDocument();
  expect(screen.getByRole("cell", { name: /Действующее поколение #7/ })).toBeInTheDocument();
  expect(screen.getByRole("alert")).toHaveTextContent("Ожидание ресурсного допуска");
});

it("saves actual metadata controls, date-free list/gate config and locked maturity without starting work", async () => {
  render(<AlgorithmsPage />);
  const dialog = await configure();
  expect(dialog.queryByLabelText("Начало периода сигналов")).not.toBeInTheDocument();
  expect(dialog.queryByLabelText("Конец периода сигналов")).not.toBeInTheDocument();
  expect(dialog.getByLabelText("Зрелость (фиксирована)")).toBeDisabled();
  expect(dialog.getByLabelText("Зрелость (фиксирована)")).toHaveValue(0);
  expect(dialog.getByLabelText("Минимальное среднее матожидание, R")).toHaveValue(0.15);
  expect(dialog.getByLabelText("Baseline: Новый порог")).toHaveValue("2.5");
  fireEvent.change(dialog.getByLabelText("Последние N дней"), { target: { value: "10" } });
  fireEvent.change(dialog.getByLabelText("Минимум сетапов в день — список профилей"), { target: { value: "0, 0.6, 4" } });
  fireEvent.change(dialog.getByLabelText("Baseline: Новый порог"), { target: { value: "3.75" } });
  await userEvent.click(dialog.getByRole("button", { name: "Сохранить непрерывное исследование" }));
  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  const put = requests.find((request) => request.path === "/v1/continuous-studies/future" && request.method === "PUT")!;
  const config = put.body as ContinuousConfig;
  expect(config).toMatchObject({ lookbackDays: 10, minSetupsPerDay: [0, 0.6, 4], minMeanExpectancy: 0.15, adaptiveEnabled: false,
    study: { algorithmParams: { futureThreshold: "3.75", opaqueFixed: "kept" }, algorithmSearch: { futureThreshold: ["1.5", "2.5"] }, instruments: [{ symbol: "OLDUSDT" }] } });
  for (const key of ["fromUnix", "toUnix", "minSetupsPerDay", "setupMaturity"]) expect(config.study).not.toHaveProperty(key);
  expect(requests.filter((request) => request.method !== "GET" && !request.path.startsWith("/v1/optimization-drafts/"))).toHaveLength(1);
  expect(screen.getByRole("checkbox", { name: "Разрешить адаптивное live-сканирование" })).not.toBeChecked();
  expect(screen.getByRole("checkbox", { name: "Включён для live-сканирования" })).not.toBeChecked();
});

it("shares common draft fields in both directions while preserving mode-specific values", async () => {
  render(<AlgorithmsPage />);
  await userEvent.click(await screen.findByRole("button", { name: "Оптимизировать Мой алгоритм" }));
  let manual = within(await screen.findByRole("dialog", { name: "Оптимизация · Мой алгоритм" }));
  const manualBaseline = await manual.findByLabelText("Baseline: Новый порог");
  fireEvent.change(manual.getByLabelText("Начало периода сигналов"), { target: { value: "2026-10-05T01:02" } });
  fireEvent.change(manual.getByLabelText("Направление сделки"), { target: { value: "long" } });
  fireEvent.change(manual.getByLabelText("Горизонт сделки, мин (фиксирован)"), { target: { value: "600" } });
  fireEvent.change(manualBaseline, { target: { value: "7.5" } });
  fireEvent.blur(manualBaseline);
  await waitFor(() => expect(requests.filter((request) => request.path === "/v1/optimization-drafts/future" && request.method === "PUT")).toHaveLength(1));
  await userEvent.click(manual.getByRole("button", { name: "Закрыть" }));

  const continuous = await configure();
  expect(continuous.getByLabelText("Направление сделки")).toHaveValue("long");
  expect(continuous.getByLabelText("Горизонт сделки, мин (фиксирован)")).toHaveValue(600);
  expect(continuous.getByLabelText("Baseline: Новый порог")).toHaveValue("7.5");
  fireEvent.change(continuous.getByLabelText("Последние N дней"), { target: { value: "17" } });
  const tp = continuous.getByLabelText("Уровни TP, %");
  fireEvent.change(tp, { target: { value: "5, 9" } });
  fireEvent.blur(tp);
  await waitFor(() => expect(requests.filter((request) => request.path === "/v1/optimization-drafts/future" && request.method === "PUT")).toHaveLength(2));
  expect(drafts.future).toMatchObject({
    shared: { scope: { direction: "long", horizonMinutes: "600", tpLevels: "5, 9" } },
    manual: { from: "2026-10-05T01:02" },
    continuous: { lookbackDays: "17" },
  });
  await userEvent.click(continuous.getByRole("button", { name: "Закрыть" }));

  await userEvent.click(screen.getByRole("button", { name: "Оптимизировать Мой алгоритм" }));
  manual = within(await screen.findByRole("dialog", { name: "Оптимизация · Мой алгоритм" }));
  expect(await manual.findByLabelText("Начало периода сигналов")).toHaveValue("2026-10-05T01:02");
  expect(manual.getByLabelText("Направление сделки")).toHaveValue("long");
  expect(manual.getByLabelText("Уровни TP, %")).toHaveValue("5, 9");
  expect(manual.queryByLabelText("Последние N дней")).not.toBeInTheDocument();
});

it("keeps adaptive permission independent and drains only through the common algorithm stop", async () => {
  render(<AlgorithmsPage />);
  const dialog = await configure();
  await userEvent.click(dialog.getByRole("button", { name: "Сохранить непрерывное исследование" }));
  await screen.findByRole("button", { name: "Запустить непрерывное исследование" });
  await userEvent.click(screen.getByRole("checkbox", { name: "Разрешить адаптивное live-сканирование" }));
  await userEvent.click(screen.getByRole("button", { name: "Сохранить разрешение адаптивного live" }));
  await screen.findByText("Разрешение адаптивного live сохранено.");
  expect(process?.config?.adaptiveEnabled).toBe(true);
  expect(requests.some((request) => request.path.endsWith("/start"))).toBe(false);
  await userEvent.click(screen.getByRole("button", { name: "Запустить непрерывное исследование" }));
  await screen.findByText("Непрерывное исследование включено");
  expect(screen.getByText("Адаптивный live разрешён · ожидание пригодного профиля.")).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "Остановить непрерывное исследование" }));
  await screen.findByText("Останавливается");
  expect(screen.getByRole("button", { name: "Остановка запрошена · ожидаем drain" })).toBeDisabled();
  expect(requests.filter((request) => request.path.endsWith("/stop"))).toHaveLength(1);
  expect(requests.some((request) => request.path.includes("/cancel") || request.method === "DELETE")).toBe(false);
  expect(screen.getByRole("checkbox", { name: "Включён для live-сканирования" })).not.toBeChecked();
});

it("renames with an isolated partial PUT and never overwrites unsaved fixed parameters", async () => {
  render(<AlgorithmsPage />);
  const input = await screen.findByRole("spinbutton", { name: "Новый порог (%)" });
  fireEvent.change(input, { target: { value: "8.5" } });
  fireEvent.change(screen.getByLabelText("Внешнее название алгоритма"), { target: { value: "  <b>Мой профиль</b>  " } });
  await userEvent.click(screen.getByRole("button", { name: "Сохранить название" }));
  await screen.findByText("Название алгоритма сохранено.");
  expect(requests.find((request) => request.method === "PUT")?.body).toEqual({ externalName: "<b>Мой профиль</b>" });
  expect(screen.getByRole("tab", { name: "<b>Мой профиль</b>" })).toBeInTheDocument();
  expect(input).toHaveValue(8.5);
  await userEvent.click(screen.getByRole("button", { name: "Сохранить алгоритм" }));
  await screen.findByText("Настройки алгоритма сохранены.");
  expect(requests.filter((request) => request.method === "PUT")[1]?.body).toEqual({ params: { futureThreshold: "8.5", opaqueFixed: "kept" }, enabled: false });
});

it("shows unavailable rather than pretending the endpoint works and preserves fixed/manual controls", async () => {
  unavailable = true;
  render(<AlgorithmsPage />);
  await screen.findByText(/Непрерывные исследования пока недоступны на сервере/);
  expect(screen.getByRole("button", { name: "Настроить непрерывное исследование" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Запустить непрерывное исследование" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Сохранить алгоритм" })).toBeEnabled();
  expect(screen.getByRole("button", { name: "Оптимизировать Мой алгоритм" })).toBeEnabled();
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it("shows active and completed continuous history with structured origin, without a row/detail stop, and permits manual work", async () => {
  studies = [study(17, "continuous", "running"), study(16, "continuous", "completed")];
  render(<AlgorithmsPage />);
  const table = within(await screen.findByRole("table", { name: "Исследования · Мой алгоритм" }));
  await table.findByRole("button", { name: "Результат исследования 17" });
  expect(table.getAllByText("Непрерывное")).toHaveLength(2);
  expect(table.queryByRole("button", { name: "Остановить исследование 17" })).not.toBeInTheDocument();
  expect(table.getByRole("button", { name: "Удалить исследование 16" })).toBeInTheDocument();
  await userEvent.click(table.getByRole("button", { name: "Результат исследования 17" }));
  const detail = within(await screen.findByRole("dialog", { name: "Исследование #17 · Мой алгоритм" }));
  await detail.findByText("Непрерывное");
  expect(detail.queryByRole("button", { name: "Остановить исследование" })).not.toBeInTheDocument();
  await userEvent.click(detail.getByRole("button", { name: "Закрыть" }));
  await userEvent.click(screen.getByRole("button", { name: "Оптимизировать Мой алгоритм" }));
  const manual = within(await screen.findByRole("dialog", { name: "Оптимизация · Мой алгоритм" }));
  await waitFor(() => expect(manual.getByRole("button", { name: "Начать исследование" })).toBeEnabled());
  expect(requests.some((request) => request.path.endsWith("/cancel"))).toBe(false);
});

it.each(["", "x".repeat(121), "bad\u0001name"])("rejects invalid external names without a write", async (name) => {
  render(<AlgorithmsPage />);
  fireEvent.change(await screen.findByLabelText("Внешнее название алгоритма"), { target: { value: name } });
  await userEvent.click(screen.getByRole("button", { name: "Сохранить название" }));
  await screen.findByText("Название: от 1 до 120 символов, без управляющих символов.");
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});
it("accepts 120 Unicode characters and Enter saves the name, not the fixed settings form", async () => {
  render(<AlgorithmsPage />);
  const input = await screen.findByLabelText("Внешнее название алгоритма");
  const name = "𐐷".repeat(120);
  fireEvent.change(input, { target: { value: name } });
  fireEvent.keyDown(input, { key: "Enter" });
  await screen.findByText("Название алгоритма сохранено.");
  expect(requests.filter((request) => request.method !== "GET")).toEqual([{ path: "/v1/algorithms/future", method: "PUT", body: { externalName: name } }]);
});
it("observes without creating work and releases every polling timer on unmount", async () => {
  vi.useFakeTimers();
  const view = render(<AlgorithmsPage />);
  await act(async () => { await vi.advanceTimersByTimeAsync(0); });
  const reads = requests.filter((request) => request.path.startsWith("/v1/continuous-studies?")).length;
  await act(async () => { await vi.advanceTimersByTimeAsync(15000); });
  expect(requests.filter((request) => request.path.startsWith("/v1/continuous-studies?")).length).toBeGreaterThan(reads);
  expect(requests.every((request) => request.method === "GET")).toBe(true);
  view.unmount();
  const after = requests.length;
  await act(async () => { await vi.advanceTimersByTimeAsync(30000); });
  expect(requests).toHaveLength(after);
  expect(vi.getTimerCount()).toBe(0);
});
it("keeps an accepted profile visible through technical failure, allows common Stop and preserves completed history", async () => {
  render(<AlgorithmsPage />);
  const dialog = await configure();
  await userEvent.click(dialog.getByRole("button", { name: "Сохранить непрерывное исследование" }));
  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  if (!process?.config) throw new Error("Expected saved continuous config");
  process = { ...process, status: "error", config: { ...process.config, adaptiveEnabled: true }, phase: "analytics", error: { code: "source_unavailable", message: "Повторная попытка" },
    slots: [{ id: 1, minSetupsPerDay: 4, status: "error", activeGenerationId: 7, studyId: 42, meanExpectancy: 0.21, selectedCell: { tpPct: 14, slPct: 4, rawR: 0.32, meanR: 0.21, robustScore: 0.01 }, updatedAt: "2026-11-12T12:00:00Z" }] };
  studies = [study(42, "continuous", "completed")];
  await userEvent.click(screen.getByRole("button", { name: "Обновить статус непрерывного процесса" }));
  await screen.findByText("Адаптивный live разрешён · принято профилей: 1.");
  expect(screen.getByText("Принятое поколение сохранено")).toBeInTheDocument();
  expect(screen.getByText("SHORT · TP 14% / SL 4% · 0.32 R")).toBeInTheDocument();
  await screen.findByRole("button", { name: "Результат исследования 42" });
  await userEvent.click(screen.getByRole("button", { name: "Остановить непрерывное исследование" }));
  await screen.findByText("Останавливается");
  expect(screen.queryByText("Принятое поколение сохранено")).not.toBeInTheDocument();
  expect(screen.queryByText("SHORT · TP 14% / SL 4% · 0.32 R")).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Результат исследования 42" })).toBeInTheDocument();
  expect(requests.some((request) => request.method === "DELETE")).toBe(false);
});
it("changes only live permission during running research without revision changes, profile loss or hidden Stop/start", async () => {
  render(<AlgorithmsPage />);
  const dialog = await configure();
  fireEvent.change(dialog.getByLabelText("Последние N дней"), { target: { value: "0.5" } });
  await userEvent.click(dialog.getByRole("button", { name: "Сохранить непрерывное исследование" }));
  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  await userEvent.click(screen.getByRole("button", { name: "Запустить непрерывное исследование" }));
  await screen.findByText("Непрерывное исследование включено");
  if (!process?.config) throw new Error("Expected configured process");
  process = { ...process, activeStudyId: 17, phase: "acquiring", config: { ...process.config, adaptiveEnabled: true }, slots: [
    { id: 1, minSetupsPerDay: 4, status: "acquiring", activeGenerationId: 7, studyId: 17, meanExpectancy: 0.21, selectedCell: { tpPct: 14, slPct: 4, rawR: 0.32, meanR: 0.21, robustScore: 0.01 }, updatedAt: "2026-11-12T12:00:00Z" },
  ] };
  const before = process;
  await userEvent.click(screen.getByRole("button", { name: "Обновить статус непрерывного процесса" }));
  await screen.findByText("Адаптивный live разрешён · принято профилей: 1.");
  await userEvent.click(screen.getByRole("checkbox", { name: "Разрешить адаптивное live-сканирование" }));
  await userEvent.click(screen.getByRole("button", { name: "Сохранить разрешение адаптивного live" }));
  await screen.findByText("Разрешение адаптивного live сохранено.");
  expect(process).toEqual({ ...before, config: { ...before.config, adaptiveEnabled: false } });
  expect(process?.configRevision).toBe(1);
  expect(requests.filter((request) => request.path === "/v1/continuous-studies/future" && request.method === "PUT").at(-1)?.body).toEqual({ ...before.config, adaptiveEnabled: false });
  expect(requests.filter((request) => request.method === "POST").map((request) => request.path)).toEqual(["/v1/continuous-studies/future/start"]);
  expect(screen.getByText("Непрерывное исследование включено")).toBeInTheDocument();
  expect(screen.getByText("Принятое поколение сохранено")).toBeInTheDocument();
  expect(screen.getByRole("checkbox", { name: "Включён для live-сканирования" })).not.toBeChecked();
});
