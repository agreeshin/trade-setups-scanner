import { StrictMode } from "react";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { actualDetailFixture, actualReportFixture, actualSetupFixture, studyActualFixture } from "../../test/study-actual-fixture";
import { applyInterfaceSettings, DEFAULT_INTERFACE_SETTINGS, formatUnix } from "../lib/datetime-format";
import type { OptimizationStudy } from "../lib/optimization-api";
import { refreshStudyActual, type StudyActualDetail } from "../lib/study-actual-api";
import { OptimizationStudyDialog } from "./algorithm-optimizer";
import { StudyActualSection } from "./study-actual-section";
import type { OptimizationController } from "./use-optimization-studies";

const algorithm = { id: "spike", version: "1", name: "Всплеск", description: "", fields: [], params: {}, enabled: false };
const progress = { phase: "done", evaluatedCandidates: 2, scorableCandidates: 2, maxTrials: 10, elapsedSeconds: 60, bestScore: 0.5 };
const study: OptimizationStudy = { id: 49, status: "completed", origin: "continuous", spec: { algorithmId: "spike", algorithmVersion: "1", algorithmParams: {}, algorithmSearch: {},
  analyticsParams: { clusterEnabled: true, clusterGapMinutes: 480, timeShiftMinutes: 0, priceChangePct: null }, analyticsSearch: {}, instruments: [], fromUnix: 1794000000, toUnix: 1794086400,
  direction: "short", setupMaturity: 0, horizonMinutes: 10080, tpLevels: [4], slLevels: [4], minClosedPerCell: 1, seed: 1, maxTrials: 10, maxSeconds: 60 },
  progress, result: null, error: null, createdAt: "2026-11-01T00:00:00Z", updatedAt: "2026-11-01T00:01:00Z", events: [] };
const controller: OptimizationController = { byAlgorithm: {}, latestByAlgorithm: {}, active: null, ready: true, error: null, creating: false, cancelling: null, deleting: null, revision: 0,
  start: vi.fn(() => Promise.resolve(true)), stop: vi.fn(() => Promise.resolve()), remove: vi.fn(() => Promise.resolve(true)), clearError: vi.fn(), refresh: vi.fn() };
const json = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status });
let actual: StudyActualDetail;
let read: () => Response | Promise<Response>;
let page: (offset: number) => Response | Promise<Response>;
const requests: { path: string; method: string; signal?: AbortSignal | null }[] = [];
const settle = async (ms = 0) => { await act(async () => { await vi.advanceTimersByTimeAsync(ms); }); };
const listButton = () => screen.getByRole("button", { name: "Посмотреть список сетапов после исследования" });
const pageCalls = () => requests.filter((request) => request.path.includes("/actual/setups?"));

beforeEach(() => {
  vi.useFakeTimers(); requests.length = 0; actual = actualDetailFixture();
  applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "UTC" });
  vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
  read = () => json(actual);
  page = (offset) => json({ studyId: 49, jobId: actual.jobId, revision: actual.revision, retrospective: true, total: 100000, limit: 50, offset,
    setups: Array.from({ length: 50 }, (_, index) => actualSetupFixture(offset + index + 1)) });
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    const method = init?.method ?? "GET"; requests.push({ path, method, ...(init?.signal ? { signal: init.signal } : {}) });
    if (path === "/v1/optimization-studies/49") return Promise.resolve(json(study));
    if (path === "/v1/optimization-studies/49/actual") return Promise.resolve(read());
    if (path === "/v1/optimization-studies/49/actual/refresh") {
      actual = { ...studyActualFixture(49, { status: "queued", stage: "queued", revision: 2, jobId: "actual-49-2" }), report: null }; return Promise.resolve(json(actual, 202));
    }
    if (path.includes("/actual/setups?")) return Promise.resolve(page(Number(new URL(path, "http://localhost").searchParams.get("offset"))));
    return Promise.resolve(json({}, 404));
  }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); applyInterfaceSettings(DEFAULT_INTERFACE_SETTINGS); });
function modal() {
  return render(<><button id="return-to-history">История</button><OptimizationStudyDialog id={49} algorithm={algorithm} controller={controller} open onOpenChange={vi.fn()} returnFocusId="return-to-history" onDelete={vi.fn()} /></>);
}

it("puts the exact list button and chart immediately after the ID dialog header and before existing result details", async () => {
  modal(); await settle();
  const dialog = screen.getByRole("dialog", { name: "Исследование #49 · Всплеск" });
  const section = within(dialog).getByRole("region", { name: "Матожидание после исследования" });
  expect(dialog.querySelector('[data-slot="dialog-header"]')?.nextElementSibling).toBe(section);
  expect(section.firstElementChild).toBe(listButton());
  expect(listButton().nextElementSibling).toBe(screen.getByTestId("study-actual-chart"));
  const result = within(dialog).getByRole("region", { name: "Статус исследования" });
  expect(section.compareDocumentPosition(result) & Node.DOCUMENT_POSITION_FOLLOWING).not.toBe(0);
  expect(requests.every((request) => request.method === "GET")).toBe(true); expect(pageCalls()).toHaveLength(0);
});

it("does not calculate or show chart/list controls before the first successful manual job, including StrictMode", async () => {
  actual = { ...studyActualFixture(), report: null };
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
  expect(within(table).getAllByRole("row")).toHaveLength(51);
  const cells = within(within(table).getAllByRole("row")[1]!).getAllByRole("cell");
  expect(cells.map((cell) => cell.textContent)).toEqual(["ACTUAL1USDT", formatUnix(actualSetupFixture().setupTimeUnix), "100", "90", "105", formatUnix(actualSetupFixture().closeTimeUnix!), "90", "+2.00R", "TP", "№49"]);
  expect(cells[3]).toHaveClass("text-success"); expect(cells[4]).toHaveClass("text-danger"); expect(cells[7]).toHaveClass("text-success");
  expect(pageCalls()).toHaveLength(1); expect(pageCalls()[0]?.path).toContain("revision=1&offset=0&limit=50");
  expect(table.closest("div")).toHaveClass("overflow-x-auto"); expect(dialog).toHaveClass("min-w-0");
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
  actual = { ...studyActualFixture(49, { status: "queued", stage: "queued", revision: 2, jobId: "actual-49-2" }), report: null };
  await settle(3000);
  expect(screen.queryByTestId("study-actual-chart")).not.toBeInTheDocument(); expect(screen.queryByRole("dialog", { name: "Сетапы после исследования #49" })).not.toBeInTheDocument();
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
  expect(screen.queryByTestId("study-actual-chart")).not.toBeInTheDocument();
  await act(async () => { resolveOld(json(old)); await Promise.resolve(); });
  expect(screen.queryByTestId("study-actual-chart")).not.toBeInTheDocument();
  read = () => json(actual); await settle(3000);
  expect(screen.getByText(/Предыдущие производные результаты удалены/)).toBeInTheDocument();
  expect(requests.filter((request) => request.method === "POST")).toHaveLength(1);
});

it("reconciles stale page revision with read-only state discovery and hides the deleted pair", async () => {
  modal(); await settle();
  page = () => { actual = { ...studyActualFixture(49, { status: "running", stage: "replaying", revision: 2, jobId: "other-tab" }), report: null }; return json({ error: "study_actual_revision_changed" }, 409); };
  fireEvent.click(listButton()); await settle();
  expect(screen.queryByTestId("study-actual-chart")).not.toBeInTheDocument(); expect(screen.getByText(/Выполняется расчёт после исследования/)).toBeInTheDocument();
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
  act(() => { applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "Asia/Tokyo" }); });
  const table = screen.getByRole("table", { name: "Сетапы после исследования" });
  expect(table.querySelector("caption")).toHaveTextContent("Asia/Tokyo");
  expect(within(table).getAllByRole("row")[1]).toHaveTextContent(formatUnix(actualSetupFixture().setupTimeUnix));
});
