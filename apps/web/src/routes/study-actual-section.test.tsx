import { StrictMode } from "react";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { actualDetailFixture, actualReportFixture, actualSetupFixture, originalReportFixture, studyActualFixture } from "../../test/study-actual-fixture";
import { applyInterfaceSettings, DEFAULT_INTERFACE_SETTINGS, formatUnix } from "../lib/datetime-format";
import { summarizeOptimizationStudy, type OptimizationStudy } from "../lib/optimization-api";
import { refreshStudyActual, type StudyActualDetail } from "../lib/study-actual-api";
import { AlgorithmOptimizer, OptimizationStudyDialog } from "./algorithm-optimizer";
import { StudyActualSection } from "./study-actual-section";
import type { OptimizationController } from "./use-optimization-studies";

const algorithm = { id: "spike", version: "1", name: "Всплеск", description: "", fields: [], params: {}, enabled: false };
const progress = { phase: "done", evaluatedCandidates: 2, scorableCandidates: 2, maxTrials: 10, elapsedSeconds: 60, bestScore: 0.5 };
const study: OptimizationStudy = { id: 49, status: "completed", origin: "continuous", spec: { algorithmId: "spike", algorithmVersion: "1", algorithmParams: {}, algorithmSearch: {},
  analyticsParams: { clusterEnabled: true, clusterGapMinutes: 480, timeShiftMinutes: 0, priceChangePct: null }, analyticsSearch: {}, instruments: [], fromUnix: 1794000000, toUnix: 1794086400,
  direction: "short", setupMaturity: 0, horizonMinutes: 10080, tpLevels: [4], slLevels: [4], minClosedPerCell: 1, seed: 1, maxTrials: 10, maxSeconds: 60 },
  progress, result: null, error: null, createdAt: "2026-11-01T00:00:00Z", updatedAt: "2026-11-01T00:01:00Z", events: [] };
const controller: OptimizationController = { byAlgorithm: {}, latestByAlgorithm: {}, active: null, ready: true, error: null, creating: false, cancelling: null, deleting: null, controlling: null, revision: 0,
  pause: vi.fn(() => Promise.resolve(true)), play: vi.fn(() => Promise.resolve(true)),
  start: vi.fn(() => Promise.resolve(true)), stop: vi.fn(() => Promise.resolve()), remove: vi.fn(() => Promise.resolve(true)), clearError: vi.fn(), refresh: vi.fn() };
const json = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status });
let actual: StudyActualDetail;
let selectedStudy: OptimizationStudy;
let read: (timeZone: string) => Response | Promise<Response>;
let page: (offset: number, hideGateFailed: boolean) => Response | Promise<Response>;
const requests: { path: string; method: string; signal?: AbortSignal | null }[] = [];
const settle = async (ms = 0) => { await act(async () => { await vi.advanceTimersByTimeAsync(ms); }); };
const listButton = () => screen.getByRole("button", { name: "Посмотреть список сетапов после исследования" });
const pageCalls = () => requests.filter((request) => request.path.includes("/actual/setups?"));

beforeEach(() => {
  vi.useFakeTimers(); requests.length = 0; actual = actualDetailFixture(); selectedStudy = study;
  applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "UTC" });
  vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
  read = () => json(actual);
  page = (offset) => json({ studyId: 49, jobId: actual.jobId, revision: actual.revision, retrospective: true, total: 100000, limit: 50, offset,
    setups: Array.from({ length: 50 }, (_, index) => actualSetupFixture(offset + index + 1)) });
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    const method = init?.method ?? "GET"; requests.push({ path, method, ...(init?.signal ? { signal: init.signal } : {}) });
    if (path === "/v1/optimization-studies/49") return Promise.resolve(json(selectedStudy));
    if (new URL(path, "http://localhost").pathname === "/v1/optimization-studies/49/actual") return Promise.resolve(read(new URL(path, "http://localhost").searchParams.get("timeZone") ?? "UTC"));
    if (path === "/v1/optimization-studies/49/actual/refresh") {
      actual = { ...studyActualFixture(49, { status: "queued", stage: "queued", revision: 2, jobId: "actual-49-2" }), report: null, originalReport: originalReportFixture() }; return Promise.resolve(json(actual, 202));
    }
    if (path.includes("/actual/setups?")) {
      const query = new URL(path, "http://localhost").searchParams;
      return Promise.resolve(page(Number(query.get("offset")), query.get("hideGateFailed") !== "false"));
    }
    return Promise.resolve(json({}, 404));
  }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); applyInterfaceSettings(DEFAULT_INTERFACE_SETTINGS); });
function modal() {
  return render(<><button id="return-to-history">История</button><OptimizationStudyDialog id={49} algorithm={algorithm} controller={controller} open onOpenChange={vi.fn()} returnFocusId="return-to-history" onDelete={vi.fn()} /></>);
}
function zonedDetail(timeZone: "Asia/Tokyo" | "Asia/Kathmandu"): StudyActualDetail {
  const base = actualDetailFixture(); const original = originalReportFixture();
  const dayUnix = Date.parse(timeZone === "Asia/Tokyo" ? "2026-11-19T15:00:00Z" : "2026-11-19T18:15:00Z") / 1000;
  return { ...base, report: { ...base.report!, display: { ...base.report!.display!, timeZone, startDayUnix: dayUnix,
    daily: [{ dayUnix, date: "2026-11-20", sumR: 2, meanR: 2 / 9, closedCount: 9, tpCount: 4, slCount: 5 }] } },
    originalReport: { ...original, display: { ...original.display!, timeZone, startDayUnix: dayUnix,
      daily: [{ dayUnix, date: "2026-11-20", sumR: -0.5, meanR: -0.5 / 7, closedCount: 7, tpCount: 3, slCount: 4 }] } } };
}

it.each(["not_calculated", "queued", "running", "failed"] as const)("renders original-only history/model for %s with no post controls or automatic computation", async (status) => {
  actual = { ...studyActualFixture(49, { status, stage: status === "not_calculated" ? null : status === "running" ? "replaying" : status }),
    report: null, originalReport: originalReportFixture() };
  modal(); await settle();
  const chart = screen.getByTestId("study-actual-chart");
  expect(chart.querySelectorAll('circle[data-phase="observed"]')).toHaveLength(2);
  expect(chart.querySelector('[data-date="2026-11-20"]')).toHaveAttribute("data-mean-r", "-0.5");
  expect(screen.getByTestId("study-original-end-value")).toHaveTextContent("+0.80R");
  expect(screen.getByTestId("study-combined-unavailable")).toHaveTextContent(`post_study_${status}`);
  expect(screen.queryByTestId("study-combined-objective-line")).not.toBeInTheDocument();
  expect(screen.queryByText("Матожидание факт:")).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Посмотреть список сетапов после исследования" })).not.toBeInTheDocument();
  expect(pageCalls()).toHaveLength(0); expect(requests.every((request) => request.method === "GET")).toBe(true);
  expect(requests.some((request) => request.path === "/v1/optimization-studies/49/actual?timeZone=UTC")).toBe(true);
});

it("does not present raw replay or old UTC aggregates as exact original outcomes", async () => {
  const original = originalReportFixture();
  original.display = { ...original.display!, daily: [], historical: { availability: { status: "unavailable", reason: "original_outcome_ledger_unavailable" },
    fidelity: "unavailable", complete: false, rawReplayAvailable: true, sourceIdentity: "study:36:raw:1767" } };
  actual = { ...studyActualFixture(), report: null, originalReport: original };
  modal(); await settle();
  expect(screen.getByTestId("study-actual-chart").querySelectorAll('circle[data-phase="observed"]')).toHaveLength(0);
  expect(screen.getByText(/Исходная история графика недоступна: original_outcome_ledger_unavailable/)).toHaveTextContent("Наличие исторического запуска не подтверждает сохранённые исходы сделок.");
  expect(screen.getByText("Исходный исторический запуск: доступен.")).toBeInTheDocument();
  expect(screen.getByTestId("study-original-objective-line")).toBeInTheDocument();
  expect(screen.queryByText("Матожидание факт:")).not.toBeInTheDocument();
});

it("shows the precise independent original-report unavailable reason without a fake chart or zero", async () => {
  actual = { ...studyActualFixture(), report: null, originalReport: null, originalReportUnavailableReason: "execution_start_unavailable" };
  modal(); await settle();
  expect(screen.getByText(/Исходный график недоступен: execution_start_unavailable/)).toBeInTheDocument();
  expect(screen.queryByTestId("study-actual-chart")).not.toBeInTheDocument();
  expect(screen.queryByText("+0.00R")).not.toBeInTheDocument();
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it("keeps original-only chart through immediate refresh invalidation, queued replacement and failed refresh", async () => {
  modal(); await settle();
  await act(async () => { await refreshStudyActual(49, new AbortController().signal); });
  expect(screen.getByTestId("study-actual-chart")).toBeInTheDocument();
  expect(screen.queryByText("Матожидание факт:")).not.toBeInTheDocument();
  await settle();
  expect(screen.getByText(/Выполняется расчёт после исследования/)).toBeInTheDocument();
  actual = { ...actual, status: "failed", stage: "failed", error: { code: "fixture_failure", stage: "evaluating", message: "Calculation failed" } };
  await settle(3000);
  expect(screen.getByText("fixture_failure: Calculation failed")).toBeInTheDocument();
  expect(screen.getByTestId("study-original-end-value")).toHaveTextContent("+0.80R");
  expect(screen.getByTestId("study-actual-chart").querySelector('[data-date="2026-11-20"]')).toHaveAttribute("data-mean-r", "-0.5");
  expect(screen.queryByRole("button", { name: "Посмотреть список сетапов после исследования" })).not.toBeInTheDocument();
  expect(requests.filter((request) => request.method === "POST")).toHaveLength(1);
});

it("replaces same-revision display reports on zone changes and fences late responses from the old zone", async () => {
  modal(); await settle();
  let resolveOld: (response: Response) => void = () => { throw new Error("not requested"); };
  const old = actual;
  read = (timeZone) => timeZone === "UTC" ? new Promise<Response>((resolve) => { resolveOld = resolve; }) : json(zonedDetail("Asia/Kathmandu"));
  await settle(3000);
  const pending = requests.at(-1)!;
  act(() => { applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "Asia/Kathmandu" }); });
  expect(screen.queryByTestId("study-actual-chart")).not.toBeInTheDocument();
  await settle();
  const chart = screen.getByTestId("study-actual-chart");
  expect(chart).toHaveAttribute("data-time-zone", "Asia/Kathmandu");
  expect(chart.querySelector('circle[data-phase="observed"]')).toHaveAttribute("data-closed", "9");
  expect(chart.querySelector('circle[data-phase="observed"]')).toHaveAttribute("data-sum-r", "2");
  expect(pending.signal?.aborted).toBe(true);
  await act(async () => { resolveOld(json(old)); await Promise.resolve(); });
  expect(screen.getByTestId("study-actual-chart")).toHaveAttribute("data-time-zone", "Asia/Kathmandu");
  expect(screen.getByTestId("study-original-end-value")).toHaveTextContent("+0.80R");
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it("puts factual mean/counts left and the exact list button right above the chart, without old visible modal copy", async () => {
  modal(); await settle();
  const dialog = screen.getByRole("dialog", { name: "Исследование #49 · Всплеск" });
  const section = within(dialog).getByRole("region", { name: "Матожидание после исследования" });
  expect(dialog.querySelector('[data-slot="dialog-header"]')?.nextElementSibling).toBe(section);
  const toolbar = section.firstElementChild!;
  expect(toolbar).toHaveClass("flex", "flex-wrap", "justify-between");
  expect(toolbar.firstElementChild).toHaveTextContent("Матожидание факт:+1.25R123/176");
  expect(toolbar.lastElementChild).toBe(listButton());
  expect(listButton()).toHaveClass("ml-auto", "max-w-full", "whitespace-normal");
  expect(within(toolbar as HTMLElement).getByTitle("Закрытые сделки / все сетапы")).toHaveTextContent("123/176");
  expect(toolbar.nextElementSibling).toBe(screen.getByTestId("study-actual-chart"));
  expect(dialog.querySelector('[data-slot="dialog-header"]')).toHaveTextContent("Исследование #49 · ВсплескЗавершеноНепрерывное");
  expect(within(dialog).getAllByText(/Исследование #49/)).toHaveLength(1);
  expect(within(dialog).queryByText("Сохранённое исследование. Просмотр и закрытие окна не меняют серверную работу или настройки.")).not.toBeInTheDocument();
  expect(within(dialog).queryByText(/Новое историческое воспроизведение · ревизия/)).not.toBeInTheDocument();
  expect(within(dialog).queryByText(/Исходная модель, без переобучения по новым исходам/)).not.toBeInTheDocument();
  expect(within(dialog).getByText(/^Жёлтая линия:/)).toHaveTextContent("Смесь · λ 0.7 · Вероятность TP с памятью · v1 · H 14 дней · κ 10");
  const result = within(dialog).getByRole("region", { name: "Статус исследования" });
  expect(section.compareDocumentPosition(result) & Node.DOCUMENT_POSITION_FOLLOWING).not.toBe(0);
  expect(within(result).queryByRole("heading", { name: /Исследование #/ })).not.toBeInTheDocument();
  expect(within(result).queryByText("Непрерывное")).not.toBeInTheDocument();
  expect(within(result).queryByRole("button", { name: "Пояснение" })).not.toBeInTheDocument();
  expect(requests.every((request) => request.method === "GET")).toBe(true); expect(pageCalls()).toHaveLength(0);
});

it("shows the API last setup below factual mean/counts using interface formats, with no setup-page request", async () => {
  actual = { ...actual, lastSetupAt: "2026-11-20T17:45:00.125Z" };
  modal(); await settle();
  const timestamp = screen.getByLabelText("Последний включённый сетап");
  expect(timestamp).toHaveTextContent("20.11.2026, 17:45:00");
  expect(timestamp).toHaveClass("basis-full");
  const count = requests.length;
  act(() => { applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "UTC", dateOrder: "mdy", dateSeparator: "/", yearDigits: 2, time24: false }); });
  expect(timestamp).toHaveTextContent("11/20/26, 05:45:00 PM");
  await settle(); expect(requests).toHaveLength(count); expect(pageCalls()).toHaveLength(0);
  expect(actual.lastSetupAt).toBe("2026-11-20T17:45:00.125Z");
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it.each([null, undefined])("shows an unavailable last setup %s as a dash in actual detail, never cutoff or job time", async (lastSetupAt) => {
  actual = { ...actual, ...(lastSetupAt === undefined ? {} : { lastSetupAt }) };
  modal(); await settle();
  expect(screen.getByLabelText("Последний включённый сетап")).toHaveTextContent(/^—$/);
  expect(pageCalls()).toHaveLength(0); expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it.each(["mean", "forecast", "blend"] as const)("keeps the saved %s objective caption and places a separator immediately after it", async (mode) => {
  actual = actualDetailFixture(49, { report: actualReportFixture({ originalObjective: {
    version: 1, trendMode: mode, trendBendDays: "30", trendBlendLambda: 0.6,
    forecast: { method: "trend", version: 1, bendDays: "30" },
  } }) });
  modal(); await settle();
  const caption = screen.getByText(/^Жёлтая линия:/);
  expect(caption).toHaveTextContent(mode === "mean" ? "Жёлтая линия: Среднее за исходный период."
    : `Жёлтая линия: ${mode === "blend" ? "Смесь · λ 0.6 · " : ""}Прогноз по тренду · перегиб 30 дней · v1.`);
  expect(caption.nextElementSibling).toHaveAttribute("data-slot", "separator");
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it.each([
  ["queued", "В очереди"], ["running", "Выполняется"], ["pausing", "Постановка на паузу"], ["paused", "На паузе"], ["cancel_requested", "Отмена запрошена"],
  ["completed", "Завершено"], ["cancelled", "Отменено"], ["failed", "Ошибка исследования"],
] as const)("shows truthful header badges and refresh only for an active saved %s study", async (status, label) => {
  selectedStudy = { ...study, status };
  modal(); await settle();
  const dialog = screen.getByRole("dialog", { name: "Исследование #49 · Всплеск" });
  const header = dialog.querySelector('[data-slot="dialog-header"]')!;
  expect(header).toHaveTextContent(label); expect(header).toHaveTextContent("Непрерывное");
  const active = ["queued", "running", "pausing", "paused", "cancel_requested"].includes(status);
  expect(within(dialog).queryByRole("button", { name: "Обновить статус" }) !== null).toBe(active);
  expect(within(dialog).queryByRole("button", { name: "Остановить исследование" })).not.toBeInTheDocument();
  if (active) expect(within(dialog).getByText("Непрерывное исследование останавливается только общей командой алгоритма.")).toBeInTheDocument();
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it.each(["completed", "running"] as const)("retains the sole ID/status/origin in the optimizer result header for %s manual studies", async (status) => {
  selectedStudy = { ...study, status, origin: "manual" };
  const optimization = { ...controller, ready: status !== "completed", byAlgorithm: { spike: summarizeOptimizationStudy(selectedStudy) } };
  render(<AlgorithmOptimizer algorithm={algorithm} controller={optimization} open onOpenChange={vi.fn()} returnFocusId="return-to-history" onDelete={vi.fn()} />);
  await settle();
  const dialog = screen.getByRole("dialog", { name: "Оптимизация · Всплеск" });
  const header = dialog.querySelector('[data-slot="dialog-header"]')!;
  expect(header).toHaveTextContent("Исследование #49"); expect(header).toHaveTextContent("Ручное");
  expect(header).toHaveTextContent(status === "completed" ? "Завершено" : "Выполняется");
  expect(within(dialog).getAllByText("Исследование #49")).toHaveLength(1);
  expect(within(dialog).queryByRole("button", { name: "Обновить статус" }) !== null).toBe(status === "running");
  if (status === "running") expect(within(dialog).getByRole("button", { name: "Остановить исследование" })).toBeEnabled();
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it("retains a paused exact-ID manual detail at 15s and observes a cross-client Play without changing its elapsed budget", async () => {
  selectedStudy = { ...study, status: "paused", origin: "manual", manualControl: { revision: 3, supportsPause: true }, progress: { ...progress, elapsedSeconds: 37 } };
  const view = modal(); await settle();
  const detailReads = () => requests.filter((request) => request.path === "/v1/optimization-studies/49").length;
  expect(detailReads()).toBe(1);
  const dialog = within(screen.getByRole("dialog", { name: "Исследование #49 · Всплеск" }));
  expect(dialog.getByText("На паузе")).toBeInTheDocument();
  expect(dialog.getByRole("button", { name: "Удалить исследование 49" })).toBeDisabled();
  await settle(14999); expect(detailReads()).toBe(1);
  selectedStudy = { ...selectedStudy, status: "queued", manualControl: { revision: 4, supportsPause: true } }; await settle(1);
  expect(detailReads()).toBe(2); expect(dialog.getByText("Ожидает очередь")).toBeInTheDocument();
  expect(dialog.getByText("Прошло, секунд").nextElementSibling).toHaveTextContent(/^37$/);
  expect(dialog.getByRole("button", { name: "Удалить исследование 49" })).toBeEnabled();
  await settle(3000); expect(detailReads()).toBe(3);
  expect(requests.every((request) => request.method === "GET")).toBe(true);
  view.unmount(); const count = requests.length; await settle(30000); expect(requests).toHaveLength(count);
});

it("does not calculate or show chart/list controls before the first successful manual job, including StrictMode", async () => {
  actual = { ...studyActualFixture(), report: null, originalReport: null };
  render(<StrictMode><StudyActualSection studyId={49} revisionTrigger={0} /></StrictMode>); await settle();
  expect(screen.queryByRole("button", { name: "Посмотреть список сетапов после исследования" })).not.toBeInTheDocument();
  expect(screen.queryByTestId("study-actual-chart")).not.toBeInTheDocument(); expect(screen.getByRole("status")).toHaveTextContent("ещё не выполнялся");
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it("opens a second modal using all ten existing Analytics columns/cells and only one bounded page", async () => {
  modal(); await settle(); fireEvent.click(listButton()); await settle();
  const dialog = screen.getByRole("dialog", { name: "Сетапы после исследования #49" });
  expect(within(dialog).getAllByRole("columnheader").map((cell) => cell.textContent)).toEqual(["Инструмент", "Время сетапа", "Вход", "TP", "SL", "Закрытие", "Цена закрытия", "Результат", "Статус", "Исследование"]);
  const table = within(dialog).getByRole("table", { name: "Сетапы после исследования" });
  const gateSwitch = within(dialog).getByRole("switch", { name: "Скрыть сетапы, которые не прошли гейт" });
  expect(gateSwitch).toBeChecked();
  expect(gateSwitch).toHaveAttribute("data-slot", "switch");
  const filterBar = gateSwitch.closest("label")!.parentElement!;
  expect(filterBar).toHaveClass("flex", "justify-end");
  expect(filterBar.nextElementSibling).toBe(table.parentElement);
  expect(within(table).getAllByRole("row")).toHaveLength(51);
  const cells = within(within(table).getAllByRole("row")[1]!).getAllByRole("cell");
  expect(cells.map((cell) => cell.textContent)).toEqual(["ACTUAL1USDT", formatUnix(actualSetupFixture().setupTimeUnix), "100", "90", "105", formatUnix(actualSetupFixture().closeTimeUnix!), "90", "+2.00R", "TP", "№49"]);
  expect(cells[3]).toHaveClass("text-success"); expect(cells[4]).toHaveClass("text-danger"); expect(cells[7]).toHaveClass("text-success");
  expect(pageCalls()).toHaveLength(1); expect(pageCalls()[0]?.path).toContain("revision=1&offset=0&limit=50&hideGateFailed=true");
  expect(table.closest("div")).toHaveClass("overflow-x-auto"); expect(dialog).toHaveClass("min-w-0");
});

it("resets a later-page offset on toggle and discards its late prior-filter response", async () => {
  let resolveOld: (response: Response) => void = () => { throw new Error("old page not requested"); };
  page = (offset, hideGateFailed) => {
    if (offset === 50) return new Promise<Response>((resolve) => { resolveOld = resolve; });
    return json({ studyId: 49, jobId: actual.jobId, revision: actual.revision, retrospective: true,
      total: hideGateFailed ? 60 : 90, limit: 50, offset,
      setups: [hideGateFailed ? actualSetupFixture(1) : { ...actualSetupFixture(2), status: "gate_failed", riskReturn: null }] });
  };
  modal(); await settle(); fireEvent.click(listButton()); await settle();
  fireEvent.click(screen.getByRole("button", { name: "Следующая страница сетапов" })); await settle();
  const pending = pageCalls().at(-1)!;
  const gateSwitch = screen.getByRole("switch", { name: "Скрыть сетапы, которые не прошли гейт" });
  fireEvent.click(gateSwitch); await settle();
  expect(gateSwitch).not.toBeChecked(); expect(pending.signal?.aborted).toBe(true);
  expect(pageCalls().at(-1)?.path).toContain("offset=0&limit=50&hideGateFailed=false");
  expect(screen.getByText("Всего сетапов: 90. Страница 1 из 2")).toBeInTheDocument();
  expect(screen.getByText("ACTUAL2USDT")).toBeInTheDocument();
  await act(async () => { resolveOld(json({ studyId: 49, jobId: actual.jobId, revision: 1, retrospective: true,
    total: 60, offset: 50, limit: 50, setups: [actualSetupFixture(99)] })); await Promise.resolve(); });
  expect(screen.queryByText("ACTUAL99USDT")).not.toBeInTheDocument();
  expect(screen.getByText("ACTUAL2USDT")).toBeInTheDocument();
  fireEvent.click(gateSwitch); await settle();
  expect(pageCalls().at(-1)?.path).toContain("offset=0&limit=50&hideGateFailed=true");
  expect(screen.getByText("ACTUAL1USDT")).toBeInTheDocument(); expect(screen.queryByText("ACTUAL2USDT")).not.toBeInTheDocument();
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it("fences a late prior-filter response even when both requests have offset zero", async () => {
  let resolveOld: (response: Response) => void = () => { throw new Error("old filter not requested"); };
  page = (offset, hideGateFailed) => hideGateFailed ? new Promise<Response>((resolve) => { resolveOld = resolve; })
    : json({ studyId: 49, jobId: actual.jobId, revision: 1, retrospective: true, total: 1, limit: 50, offset,
      setups: [{ ...actualSetupFixture(2), status: "gate_failed", riskReturn: null }] });
  modal(); await settle(); fireEvent.click(listButton()); await settle();
  const old = pageCalls()[0]!;
  fireEvent.click(screen.getByRole("switch", { name: "Скрыть сетапы, которые не прошли гейт" })); await settle();
  expect(old.signal?.aborted).toBe(true); expect(screen.getByText("ACTUAL2USDT")).toBeInTheDocument();
  await act(async () => { resolveOld(json({ studyId: 49, jobId: actual.jobId, revision: 1, retrospective: true,
    total: 1, offset: 0, limit: 50, setups: [actualSetupFixture(1)] })); await Promise.resolve(); });
  expect(screen.getByText("ACTUAL2USDT")).toBeInTheDocument(); expect(screen.queryByText("ACTUAL1USDT")).not.toBeInTheDocument();
  expect(pageCalls()).toHaveLength(2);
});

it("shows an empty filtered population for all gate failures without changing published science or summary", async () => {
  actual = { ...actual, meanR: null, lastSetupAt: "2026-11-20T17:45:00Z",
    counts: { total: 3, closed: 0, tp: 0, sl: 0, gateFailed: 3, unclosed: 0, noData: 0 } };
  const published = structuredClone(actual);
  page = (offset, hideGateFailed) => json({ studyId: 49, jobId: actual.jobId, revision: 1, retrospective: true,
    total: hideGateFailed ? 0 : 3, limit: 50, offset,
    setups: hideGateFailed ? [] : [1, 2, 3].map((id) => ({ ...actualSetupFixture(id), status: "gate_failed", riskReturn: null })) });
  modal(); await settle();
  const chart = screen.getByTestId("study-actual-chart").outerHTML;
  fireEvent.click(listButton()); await settle();
  expect(screen.getByText("Нет сетапов, соответствующих фильтру.")).toBeInTheDocument();
  expect(screen.getByText("Всего сетапов: 0. Страница 1 из 1")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Следующая страница сетапов" })).toBeDisabled();
  fireEvent.click(screen.getByRole("switch", { name: "Скрыть сетапы, которые не прошли гейт" })); await settle();
  expect(screen.getByText("Всего сетапов: 3. Страница 1 из 1")).toBeInTheDocument();
  expect(screen.getAllByText("Гейт не пройден")).toHaveLength(3);
  fireEvent.click(within(screen.getByRole("dialog", { name: "Сетапы после исследования #49" })).getByRole("button", { name: "Закрыть" })); await settle();
  expect(screen.getByText("0/3")).toBeInTheDocument(); expect(screen.getByText("Нет закрытых сделок")).toBeInTheDocument();
  expect(screen.getByLabelText("Последний включённый сетап")).toHaveTextContent("20.11.2026, 17:45:00");
  expect(screen.getByTestId("study-actual-chart").outerHTML).toBe(chart); expect(actual).toEqual(published);
  expect(requests.every((request) => request.method === "GET")).toBe(true);
  fireEvent.click(listButton()); await settle();
  expect(screen.getByRole("switch", { name: "Скрыть сетапы, которые не прошли гейт" })).toBeChecked();
});

it("replaces pages rather than accumulating them even for 100000 rows, and restores focus after closing the second modal", async () => {
  modal(); await settle(); const trigger = listButton(); fireEvent.click(trigger); await settle();
  fireEvent.click(screen.getByRole("button", { name: "Следующая страница сетапов" }));
  expect(screen.queryByText("ACTUAL1USDT")).not.toBeInTheDocument(); await settle();
  expect(screen.getByText("ACTUAL51USDT")).toBeInTheDocument(); expect(screen.queryByText("ACTUAL1USDT")).not.toBeInTheDocument();
  expect(pageCalls()).toHaveLength(2); expect(pageCalls()[1]?.path).toContain("offset=50&limit=50");
  expect(screen.getByText("Всего сетапов: 100000. Страница 2 из 2000")).toBeInTheDocument();
  fireEvent.click(within(screen.getByRole("dialog", { name: "Сетапы после исследования #49" })).getByRole("button", { name: "Закрыть" })); await settle();
  expect(trigger).toHaveFocus();
});

it("keeps factual mean/table usable when original graph assets are unavailable", async () => {
  actual = actualDetailFixture(49, { report: actualReportFixture({ sourceArtifactId: null, originalObjective: null,
    historical: { availability: { status: "unavailable", reason: "original_series_unavailable" }, daily: [] },
    projection: { availability: { status: "unavailable", reason: "original_series_unavailable" }, method: null, points: [], fittedOnPostStudyData: false } }) });
  modal(); await settle(); expect(screen.getByText("+1.25R")).toBeInTheDocument();
  expect(screen.getByText(/Исходная история графика недоступна/)).toBeInTheDocument(); expect(screen.queryByTestId("study-original-objective-line")).not.toBeInTheDocument();
  fireEvent.click(listButton()); await settle(); expect(screen.getByText("ACTUAL1USDT")).toBeInTheDocument();
});

it("retains the sealed original projection and graph with zero closed post-start trades, without a fake zero mean", async () => {
  const report = actualReportFixture();
  actual = actualDetailFixture(49, { meanR: null, counts: { total: 23, closed: 0, tp: 0, sl: 0, gateFailed: 20, unclosed: 3, noData: 0 },
    report: { ...report, actual: { daily: [{ ...report.actual.daily[0]!, meanR: null, closedCount: 0, tpCount: 0, slCount: 0 }] } } });
  modal(); await settle(); expect(screen.getByTestId("study-original-objective-line")).toBeInTheDocument();
  expect(screen.getByText("0/23")).toBeInTheDocument(); expect(screen.getByText("Нет закрытых сделок")).toBeInTheDocument(); expect(screen.queryByText("+0.00R")).not.toBeInTheDocument();
});

it("invalidates chart/list on another tab's queued revision and ignores its late old-page response", async () => {
  let resolvePage: (response: Response) => void = () => { throw new Error("page not requested"); };
  page = () => new Promise<Response>((resolve) => { resolvePage = resolve; });
  modal(); await settle(); fireEvent.click(listButton()); await settle();
  const pending = pageCalls()[0]!;
  actual = { ...studyActualFixture(49, { status: "queued", stage: "queued", revision: 2, jobId: "actual-49-2" }), report: null, originalReport: originalReportFixture() };
  await settle(3000);
  expect(screen.getByTestId("study-actual-chart")).toBeInTheDocument(); expect(screen.queryByRole("dialog", { name: "Сетапы после исследования #49" })).not.toBeInTheDocument();
  expect(pending.signal?.aborted).toBe(true);
  await act(async () => { resolvePage(json({ studyId: 49, jobId: "actual-49-1", revision: 1, retrospective: true, total: 1, offset: 0, limit: 50, setups: [actualSetupFixture()] })); await Promise.resolve(); });
  expect(screen.queryByText("ACTUAL1USDT")).not.toBeInTheDocument();
  actual = actualDetailFixture(49, { revision: 2, jobId: "actual-49-2" }); await settle(3000);
  expect(listButton()).toBeInTheDocument(); expect(pageCalls()).toHaveLength(1); expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it("invalidates immediately on accepted manual refresh and fences an old full-report response", async () => {
  modal(); await settle(); const old = actual;
  let resolveOld: (response: Response) => void = () => { throw new Error("report not requested"); };
  read = () => new Promise<Response>((resolve) => { resolveOld = resolve; });
  await act(async () => { await refreshStudyActual(49, new AbortController().signal); });
  expect(screen.getByTestId("study-actual-chart")).toBeInTheDocument();
  await act(async () => { resolveOld(json(old)); await Promise.resolve(); });
  expect(screen.getByTestId("study-actual-chart")).toBeInTheDocument();
  read = () => json(actual); await settle(3000);
  expect(screen.getByText(/Предыдущие производные результаты удалены/)).toBeInTheDocument();
  expect(requests.filter((request) => request.method === "POST")).toHaveLength(1);
});

it("reconciles stale page revision with read-only state discovery and hides the deleted pair", async () => {
  modal(); await settle();
  page = () => { actual = { ...studyActualFixture(49, { status: "running", stage: "replaying", revision: 2, jobId: "other-tab" }), report: null, originalReport: originalReportFixture() }; return json({ error: "study_actual_revision_changed" }, 409); };
  fireEvent.click(listButton()); await settle();
  expect(screen.getByTestId("study-actual-chart")).toBeInTheDocument(); expect(screen.getByText(/Выполняется расчёт после исследования/)).toBeInTheDocument();
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it.each([401, 404])("shows protected-read failure %i without graph, setup fallback, calculation or retry loop", async (status) => {
  read = () => json({ error: "unavailable" }, status); modal(); await settle();
  expect(screen.getByRole("alert")).toBeInTheDocument(); expect(screen.queryByTestId("study-actual-chart")).not.toBeInTheDocument();
  const count = requests.length; await settle(9000); expect(requests).toHaveLength(count); expect(pageCalls()).toHaveLength(0);
});

it("suspends observers while hidden, rereads on visibility restore, and releases requests/timers on close", async () => {
  const visibility = vi.spyOn(document, "visibilityState", "get");
  const view = modal(); await settle(); visibility.mockReturnValue("hidden"); fireEvent(document, new Event("visibilitychange"));
  const count = requests.length; await settle(9000); expect(requests).toHaveLength(count);
  visibility.mockReturnValue("visible"); fireEvent(document, new Event("visibilitychange")); await settle(); expect(requests.length).toBeGreaterThan(count);
  view.unmount(); const after = requests.length; await settle(9000); expect(requests).toHaveLength(after);
  expect(requests.every((request) => request.signal?.aborted)).toBe(true);
});

it("keeps responsive controls and configured-timezone cells in the paged second modal", async () => {
  vi.stubGlobal("innerWidth", 375); modal(); await settle(); expect(listButton()).toHaveClass("max-w-full", "whitespace-normal", "h-auto");
  fireEvent.click(listButton()); await settle();
  const dialog = screen.getByRole("dialog", { name: "Сетапы после исследования #49" });
  fireEvent.click(screen.getByRole("button", { name: "Следующая страница сетапов" })); await settle();
  const pagesBefore = pageCalls().length;
  read = () => json(zonedDetail("Asia/Tokyo"));
  act(() => { applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "Asia/Tokyo" }); });
  await settle();
  expect(screen.getByRole("dialog", { name: "Сетапы после исследования #49" })).toBe(dialog);
  const table = screen.getByRole("table", { name: "Сетапы после исследования" });
  expect(table.querySelector("caption")).toHaveTextContent("Asia/Tokyo");
  expect(within(table).getAllByRole("row")[1]).toHaveTextContent(formatUnix(actualSetupFixture().setupTimeUnix));
  expect(screen.getByText("ACTUAL51USDT")).toBeInTheDocument(); expect(pageCalls()).toHaveLength(pagesBefore);
});
