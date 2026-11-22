import { StrictMode } from "react";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { actualCountsFixture, studyActualFixture } from "../../test/study-actual-fixture";
import producerPagesJson from "../../test/study-actual-producer-pages.json?raw";
import { applyInterfaceSettings, DEFAULT_INTERFACE_SETTINGS } from "../lib/datetime-format";
import type { OptimizationStudySummary } from "../lib/optimization-api";
import type { StudyActualSummary } from "../lib/study-actual-api";
import { OptimizationHistory } from "./optimization-history";
import { RouteActiveContext } from "./route-active";
import type { OptimizationController } from "./use-optimization-studies";

function row(id: number, actual: StudyActualSummary | null = studyActualFixture(id)): OptimizationStudySummary {
  return { id, origin: "continuous", status: "completed", spec: { algorithmId: "spike", algorithmVersion: "1", algorithmParams: {}, algorithmSearch: {},
    analyticsParams: { clusterEnabled: true, clusterGapMinutes: 480, timeShiftMinutes: 0, priceChangePct: null }, analyticsSearch: {}, instruments: [],
    fromUnix: 1794000000, toUnix: 1794086400, direction: "short", setupMaturity: 0, horizonMinutes: 10080, tpLevels: [4], slLevels: [4],
    minClosedPerCell: 1, seed: 1, maxTrials: 10, maxSeconds: 60 }, progress: { phase: "done", evaluatedCandidates: 2, scorableCandidates: 2, maxTrials: 10, elapsedSeconds: 60, bestScore: 0.5 },
    bestScore: 0.5, forecastBest: { forecastR: 0.7, meanR: 0.1, tpPct: 4, slPct: 4 }, error: null, actual, createdAt: "2026-11-01T00:00:00Z", updatedAt: "2026-11-01T00:01:00Z" };
}
const controller: OptimizationController = { byAlgorithm: {}, latestByAlgorithm: {}, active: null, ready: true, error: null, creating: false, cancelling: null, deleting: null, revision: 0,
  start: vi.fn(() => Promise.resolve(true)), stop: vi.fn(() => Promise.resolve()), remove: vi.fn(() => Promise.resolve(true)), clearError: vi.fn(), refresh: vi.fn() };
let rows: OptimizationStudySummary[];
let post: (id: number) => Response | Promise<Response>;
let read: (id: number) => Response | Promise<Response>;
let readPage: (params: URLSearchParams) => Response | Promise<Response>;
const requests: { path: string; method: string; body: unknown; signal?: AbortSignal | null }[] = [];
const json = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status });
const pageResponse = (offset = 0, limit = 20) => json({ studies: rows.slice(offset, offset + limit), total: rows.length, limit, offset });
const button = (id = 49) => screen.getByRole("button", { name: `Пересчитать матожидание после исследования ${id}` });
const tableRow = (id = 49) => screen.getByRole("button", { name: `Результат исследования ${id}` }).closest("tr")!;
const posts = () => requests.filter((request) => request.method === "POST");
const settle = async (ms = 0) => { await act(async () => { await vi.advanceTimersByTimeAsync(ms); }); };

beforeEach(() => {
  vi.useFakeTimers(); requests.length = 0; rows = [row(49), row(48)];
  applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "UTC" });
  vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
  post = (id) => {
    const next = studyActualFixture(id, { revision: 1, jobId: `job-${id}`, status: "queued", stage: "queued", observedThroughUnix: 1795201200 });
    rows = rows.map((item) => item.id === id ? { ...item, actual: next } : item); return json(next, 202);
  };
  read = (id) => json(rows.find((item) => item.id === id)?.actual);
  readPage = (params) => pageResponse(Number(params.get("offset")), Number(params.get("limit")));
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    requests.push({ path, method, body: init?.body, ...(init?.signal ? { signal: init.signal } : {}) });
    if (path.startsWith("/v1/optimization-studies?")) {
      const params = new URL(path, "http://localhost").searchParams;
      return Promise.resolve(readPage(params));
    }
    const actual = /^\/v1\/optimization-studies\/(\d+)\/actual(\/refresh)?$/.exec(path);
    if (actual) return Promise.resolve(actual[2] ? post(Number(actual[1])) : read(Number(actual[1])));
    return Promise.resolve(json({}, 404));
  }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); applyInterfaceSettings(DEFAULT_INTERFACE_SETTINGS); });
function surface(strict = false, pageSize = 5) {
  const node = <OptimizationHistory algorithmId="spike" algorithmName="Всплеск" controller={controller} pageSize={pageSize} pagination={false} onSelect={vi.fn()} onDelete={vi.fn()} />;
  return render(strict ? <StrictMode>{node}</StrictMode> : node);
}

it("mounts under StrictMode without any calculation or per-row discovery, keeping forecast distinct from actual", async () => {
  surface(true); await settle();
  expect(screen.getByRole("columnheader", { name: "Матожидание факт" })).toBeInTheDocument();
  expect(screen.getByRole("columnheader", { name: "Лучший прогноз для ячейки" })).toBeInTheDocument();
  expect(within(tableRow()).getByText("0.70 R")).toBeInTheDocument();
  expect(within(tableRow()).getByText("Не рассчитано")).toBeInTheDocument();
  expect(posts()).toHaveLength(0); expect(requests.every((request) => request.path.includes("?"))).toBe(true);
});

it("shows the exact saved TP/SL levels from the same forecastBest cell as its forecast, independently of score and current search axes", async () => {
  rows[0] = { ...row(49), forecastBest: { forecastR: 0.87654, meanR: 1.9, tpPct: 14.125, slPct: 0.625 } };
  rows[1] = { ...row(48), forecastBest: { forecastR: -0.126, meanR: 2.7, tpPct: 8.25, slPct: 2.5 } };
  surface(); await settle();
  const first = within(tableRow()).getAllByRole("cell");
  expect(first).toHaveLength(11);
  expect(first[6]).toHaveTextContent(/^TP 14.125% \/ SL 0.625%$/);
  expect(first[7]).toHaveTextContent(/^0.50 R$/); expect(first[8]).toHaveTextContent(/^0.88 R$/);
  const second = within(tableRow(48)).getAllByRole("cell");
  expect(second[6]).toHaveTextContent(/^TP 8.25% \/ SL 2.5%$/); expect(second[8]).toHaveTextContent(/^-0.13 R$/);
  expect(screen.queryByText("TP 4% / SL 4%")).not.toBeInTheDocument();
  expect(requests).toHaveLength(1); expect(posts()).toHaveLength(0);
});

it.each([
  { label: "legacy omitted", forecastBest: undefined },
  { label: "legacy null", forecastBest: null },
  { label: "non-finite TP", forecastBest: { forecastR: 0.7, meanR: 0.1, tpPct: Number.NaN, slPct: 4 } },
  { label: "non-finite SL", forecastBest: { forecastR: 0.7, meanR: 0.1, tpPct: 4, slPct: Number.POSITIVE_INFINITY } },
])("shows a dash for a $label saved pair without inferring levels from the spec", async ({ forecastBest }) => {
  const saved = row(49);
  if (forecastBest === undefined) delete saved.forecastBest;
  else saved.forecastBest = forecastBest;
  rows[0] = saved;
  // Exercise non-finite in-memory data too; JSON serialization would replace it with null.
  const response = json({});
  vi.spyOn(response, "json").mockResolvedValue({ studies: rows, total: rows.length, limit: 5, offset: 0 });
  readPage = () => response;
  surface(); await settle();
  const cells = within(tableRow()).getAllByRole("cell");
  expect(cells[6]).toHaveTextContent(/^—$/);
  expect(cells[8]).toHaveTextContent(forecastBest == null ? /^—$/ : /^0.70 R$/);
  expect(requests).toHaveLength(1); expect(posts()).toHaveLength(0);
});

it("renders exactly two block date lines with reactive interface formats across DST while preserving UTC half-open boundaries", async () => {
  const fromUnix = Date.parse("2026-11-01T04:30:00Z") / 1000;
  const toUnix = Date.parse("2026-11-02T05:30:00Z") / 1000;
  rows[0] = { ...row(49), spec: { ...row(49).spec, fromUnix, toUnix } };
  surface(); await settle();
  const cell = within(tableRow()).getAllByRole("cell")[2]!;
  const lines = () => [...cell.children].map((line) => line.textContent);
  expect(lines()).toEqual(["01.11.2026, 04:30:00 -", "02.11.2026, 05:30:00"]);
  expect(cell.children).toHaveLength(2);
  for (const line of cell.children) expect(line).toHaveClass("block");
  expect(cell).toHaveAttribute("title", "Начало включительно; конец исключительно.");
  expect(cell.textContent).not.toMatch(/[[\]()]/);
  act(() => { applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "Asia/Kathmandu", dateOrder: "ymd", dateSeparator: "-" }); });
  expect(lines()).toEqual(["2026-11-01, 10:15:00 -", "2026-11-02, 11:15:00"]);
  act(() => { applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "America/New_York", dateOrder: "mdy", dateSeparator: "/", yearDigits: 2, time24: false }); });
  expect(lines()).toEqual(["11/01/26, 12:30:00 AM -", "11/02/26, 12:30:00 AM"]);
  expect(rows[0].spec).toMatchObject({ fromUnix, toUnix });
  expect(requests).toHaveLength(1); expect(requests[0]?.method).toBe("GET");
});

it.each([
  { label: "legacy mean defaults", settings: {}, lines: ["Среднее"] },
  { label: "inactive recipe in mean", settings: { trendMode: "mean", trendBlendLambda: 0.6, forecast: { method: "trend", version: 1, bendDays: "30" } } as const, lines: ["Среднее"] },
  { label: "legacy forecast defaults", settings: { trendMode: "forecast" } as const, lines: ["Прогноз · λ 0.7 (не применяется)", "Тренд · прямая"] },
  { label: "legacy blend bend and default lambda", settings: { trendMode: "blend", trendBendDays: "14" } as const, lines: ["Смесь · λ 0.7", "Тренд · перегиб 14 дней"] },
  { label: "explicit trend overriding inactive legacy bend", settings: { trendMode: "forecast", trendBlendLambda: 0.6, trendBendDays: "30", forecast: { method: "trend", version: 1, bendDays: "7" } } as const, lines: ["Прогноз · λ 0.6 (не применяется)", "Тренд · перегиб 7 дней"] },
  { label: "saved blend probability memory", settings: { trendMode: "blend", trendBlendLambda: 0.29, forecast: { method: "decayed_probability", version: 1, halfLifeDays: 28.5, kappa: 5, priorPolicy: "gross_break_even" } } as const, lines: ["Смесь · λ 0.29", "Вероятность TP с памятью · H 28.5 дней"] },
  { label: "pure forecast probability with inactive saved lambda", settings: { trendMode: "forecast", trendBlendLambda: 0.8, forecast: { method: "decayed_probability", version: 1, halfLifeDays: 7, kappa: 20, priorPolicy: "gross_break_even" } } as const, lines: ["Прогноз · λ 0.8 (не применяется)", "Вероятность TP с памятью · H 7 дней"] },
])("shows $label from the frozen compact spec with no extra detail or calculation request", async ({ settings, lines }) => {
  const savedSpec = { ...row(49).spec, ...settings };
  rows[0] = { ...row(49), spec: savedSpec };
  surface(); await settle();
  const cell = within(tableRow()).getAllByRole("cell")[5]!;
  for (const line of lines) expect(within(cell).getByText(line, { exact: true })).toBeInTheDocument();
  if (lines.length === 1) expect(cell).toHaveTextContent(/^Среднее$/);
  if (savedSpec.forecast?.method === "decayed_probability") {
    expect(within(cell).getByText(lines[1]!)).toHaveAttribute("title", expect.stringContaining(`κ ${savedSpec.forecast.kappa}`));
  }
  expect(requests).toHaveLength(1); expect(posts()).toHaveLength(0);
});

it.each(["queued", "running", "cancel_requested"] as const)("uses disabled trash for a %s continuous child without changing the manual Stop action", async (status) => {
  rows = [{ ...row(49), status }, { ...row(48), origin: "manual", status }];
  const stop = vi.fn(() => Promise.resolve()); const onDelete = vi.fn();
  render(<OptimizationHistory algorithmId="spike" algorithmName="Всплеск" controller={{ ...controller, stop }} pageSize={5} onSelect={vi.fn()} onDelete={onDelete} />);
  await settle();
  const continuous = within(tableRow());
  const trash = continuous.getByRole("button", { name: "Удалить исследование 49" });
  expect(trash).toBeDisabled(); expect(trash.querySelector('svg[aria-hidden="true"]')).not.toBeNull();
  expect(trash).toHaveAccessibleDescription("Удаление недоступно, пока непрерывное исследование выполняется.");
  expect(continuous.queryByText("Общая остановка в настройках алгоритма")).not.toBeInTheDocument();
  expect(continuous.queryByRole("button", { name: /Остановить исследование/ })).not.toBeInTheDocument();
  fireEvent.click(trash); expect(onDelete).not.toHaveBeenCalled(); expect(stop).not.toHaveBeenCalled();
  const manual = within(tableRow(48));
  const manualStop = manual.getByRole("button", { name: "Остановить исследование 48" });
  expect(manual.queryByRole("button", { name: /Удалить исследование/ })).not.toBeInTheDocument();
  if (status === "cancel_requested") {
    expect(manualStop).toBeDisabled(); expect(manualStop).toHaveTextContent("Остановка запрошена");
    fireEvent.click(manualStop); expect(stop).not.toHaveBeenCalled();
  } else {
    expect(manualStop).toBeEnabled(); expect(manualStop).toHaveTextContent(/^Остановить$/);
    fireEvent.click(manualStop); expect(stop).toHaveBeenCalledExactlyOnceWith(rows[1]);
  }
  expect(posts()).toHaveLength(0);
});

it.each(["completed", "cancelled", "failed"] as const)("keeps %s manual and continuous deletion enabled and routed to the existing confirmation handler", async (status) => {
  rows = [{ ...row(49), status }, { ...row(48), status, origin: "manual" }];
  const onDelete = vi.fn();
  render(<OptimizationHistory algorithmId="spike" algorithmName="Всплеск" controller={controller} pageSize={5} onSelect={vi.fn()} onDelete={onDelete} />);
  await settle();
  for (const study of rows) {
    const trash = within(tableRow(study.id)).getByRole("button", { name: `Удалить исследование ${study.id}` });
    expect(trash).toBeEnabled(); expect(trash.querySelector('svg[aria-hidden="true"]')).not.toBeNull();
    fireEvent.click(trash); expect(onDelete).toHaveBeenLastCalledWith(study);
  }
  expect(onDelete).toHaveBeenCalledTimes(2); expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it("blocks only the submitted study, spins its icon, deduplicates clicks, and allows another row in parallel", async () => {
  const pending = new Map<number, (response: Response) => void>();
  post = (id) => new Promise<Response>((resolve) => { pending.set(id, resolve); });
  const view = surface(); await settle();
  fireEvent.click(button()); fireEvent.click(button());
  expect(button()).toBeDisabled(); expect(button()).toHaveAttribute("aria-busy", "true");
  expect(button().querySelector("svg")).toHaveClass("motion-safe:animate-spin");
  expect(button(48)).toBeEnabled(); fireEvent.click(button(48));
  expect(posts()).toHaveLength(2); expect(posts().map((request) => request.body)).toEqual(["{}", "{}"]);
  view.unmount(); expect(posts().every((request) => request.signal?.aborted)).toBe(true);
  await act(async () => {
    for (const [id, resolve] of pending) resolve(json(studyActualFixture(id, { status: "queued", stage: "queued", revision: 1, jobId: `job-${id}` }), 202));
    await Promise.resolve();
  });
});

it("restores a persisted queued job after reload and observes completion using only compact page GET polling", async () => {
  rows[0] = row(49, studyActualFixture(49, { status: "queued", stage: "queued", revision: 2, jobId: "persisted" }));
  surface(); await settle(); expect(button()).toBeDisabled(); expect(within(tableRow()).getByRole("status")).toHaveTextContent("В очереди");
  cleanup(); surface(); await settle(); expect(button()).toBeDisabled();
  rows[0] = row(49, studyActualFixture(49, { status: "completed", stage: "completed", revision: 2, jobId: "persisted", counts: actualCountsFixture, meanR: -0.25 }));
  await settle(3000);
  expect(button()).toBeEnabled(); expect(within(tableRow()).getByText("-0.25R")).toHaveClass("text-danger");
  expect(within(tableRow()).getByText("123/176")).toHaveClass("text-ambient-violet");
  expect(posts()).toHaveLength(0);
  const count = requests.length; await settle(9000); expect(requests).toHaveLength(count);
});

it("uses closed/all counts, displays null as unavailable rather than zero, and exposes the exact count tooltip", async () => {
  rows[0] = row(49, studyActualFixture(49, { status: "completed", stage: "completed", counts: { total: 23, closed: 0, tp: 0, sl: 0, gateFailed: 20, unclosed: 3, noData: 0 } }));
  surface(); await settle();
  expect(within(tableRow()).getByText("0/23")).toBeInTheDocument(); expect(within(tableRow()).getByText("Нет закрытых сделок")).toBeInTheDocument();
  expect(within(tableRow()).queryByText("+0.00R")).not.toBeInTheDocument();
  fireEvent.focus(within(tableRow()).getByText("0/23"));
  expect(screen.getByRole("tooltip")).toHaveTextContent("Закрытые сделки / все сетапы");
  expect(screen.getByRole("tooltip")).toHaveTextContent("гейт не пройден: 20");
});

it("renders the compact API last setup using reactive interface timezone/date/clock settings without any extra requests", async () => {
  rows[0] = row(49, studyActualFixture(49, { status: "completed", stage: "completed", counts: actualCountsFixture, meanR: 1.25, lastSetupAt: "2026-11-20T23:30:00.125Z" }));
  surface(); await settle();
  const timestamp = within(tableRow()).getByLabelText("Последний включённый сетап");
  expect(timestamp).toHaveTextContent("20.11.2026, 23:30:00");
  const count = requests.length;
  act(() => { applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "Asia/Kathmandu", dateOrder: "ymd", dateSeparator: "-" }); });
  expect(timestamp).toHaveTextContent("2026-11-21, 05:15:00");
  act(() => { applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "America/New_York", dateOrder: "mdy", dateSeparator: "/", yearDigits: 2, time24: false }); });
  expect(timestamp).toHaveTextContent("11/20/26, 06:30:00 PM");
  await settle(); expect(requests).toHaveLength(count); expect(posts()).toHaveLength(0);
  expect(requests.every((request) => request.path.startsWith("/v1/optimization-studies?"))).toBe(true);
  expect(rows[0].actual!.lastSetupAt).toBe("2026-11-20T23:30:00.125Z");
});

it.each([null, undefined])("shows missing/legacy lastSetupAt %s as a dash, not cutoff or research metadata", async (lastSetupAt) => {
  rows[0] = row(49, studyActualFixture(49, { status: "completed", stage: "completed", counts: { total: 0, closed: 0, tp: 0, sl: 0, gateFailed: 0, unclosed: 0, noData: 0 }, observedThroughUnix: 1795201200, ...(lastSetupAt === undefined ? {} : { lastSetupAt }) }));
  surface(); await settle();
  expect(within(tableRow()).getByLabelText("Последний включённый сетап")).toHaveTextContent(/^—$/);
  expect(within(tableRow()).getByText("0/0")).toBeInTheDocument();
  expect(requests).toHaveLength(1); expect(posts()).toHaveLength(0);
});

it("shows the server last setup even without closed trades and removes it immediately during replacement", async () => {
  rows[0] = row(49, studyActualFixture(49, { status: "completed", stage: "completed", counts: { total: 23, closed: 0, tp: 0, sl: 0, gateFailed: 20, unclosed: 3, noData: 0 }, lastSetupAt: "2026-11-20T17:45:00Z" }));
  surface(); await settle();
  expect(within(tableRow()).getByLabelText("Последний включённый сетап")).toHaveTextContent("20.11.2026, 17:45:00");
  expect(within(tableRow()).getByText("0/23")).toBeInTheDocument();
  fireEvent.click(button());
  expect(within(tableRow()).getByLabelText("Последний включённый сетап")).toHaveTextContent(/^—$/);
  await settle(); expect(within(tableRow()).queryByText("20.11.2026, 17:45:00")).not.toBeInTheDocument();
  expect(posts()).toHaveLength(1);
});

it.each([1.25, 0, -0.00001])("uses the unrounded sign of server mean %s", async (meanR) => {
  rows[0] = row(49, studyActualFixture(49, { status: "completed", stage: "completed", counts: actualCountsFixture, meanR }));
  surface(); await settle();
  const text = `${meanR >= 0 ? "+" : ""}${meanR.toFixed(2)}R`;
  expect(within(tableRow()).getByText(text)).toHaveClass(meanR === 0 ? "text-muted-foreground" : meanR > 0 ? "text-success" : "text-danger");
});

it("immediately removes previous fact display when a manual replacement starts", async () => {
  rows[0] = row(49, studyActualFixture(49, { status: "completed", stage: "completed", counts: actualCountsFixture, meanR: 1.25 }));
  surface(); await settle(); expect(within(tableRow()).getByText("+1.25R")).toBeInTheDocument();
  fireEvent.click(button()); expect(within(tableRow()).queryByText("+1.25R")).not.toBeInTheDocument();
  await settle(); expect(within(tableRow()).queryByText("123/176")).not.toBeInTheDocument(); expect(button()).toBeDisabled();
});

it("adopts a same-study busy job without resubmitting", async () => {
  const current = studyActualFixture(49, { status: "running", stage: "replaying", revision: 2, jobId: "other-tab" });
  post = () => json({ error: "study_actual_busy", actual: current }, 409);
  surface(); await settle(); fireEvent.click(button()); await settle();
  expect(button()).toBeDisabled(); expect(within(tableRow()).getByRole("status")).toHaveTextContent("Исторический прогон"); expect(posts()).toHaveLength(1);
});

it("reconciles uncertain POST acknowledgement with GET and never retries POST automatically", async () => {
  post = () => { throw new Error("connection lost after commit"); };
  read = () => json(studyActualFixture(49, { status: "queued", stage: "queued", revision: 3, jobId: "ack-lost" }));
  surface(); await settle(); fireEvent.click(button()); await settle();
  expect(button()).toBeDisabled(); expect(posts()).toHaveLength(1);
  expect(requests.some((request) => request.method === "GET" && request.path === "/v1/optimization-studies/49/actual")).toBe(true);
});

it("keeps an uncertain command blocked if reconciliation fails, with an explicit GET-only recovery action", async () => {
  post = () => { throw new Error("offline"); }; read = () => { throw new Error("offline"); };
  surface(); await settle(); fireEvent.click(button()); await settle();
  expect(button()).toBeDisabled(); expect(within(tableRow()).getByRole("alert")).toHaveTextContent("Новый запуск заблокирован");
  read = () => json(studyActualFixture(49, { status: "running", stage: "evaluating", revision: 1, jobId: "recovered" }));
  fireEvent.click(screen.getByRole("button", { name: "Проверить статус расчёта" })); await settle();
  expect(posts()).toHaveLength(1); expect(within(tableRow()).getByRole("status")).toHaveTextContent("Расчёт исходов");
});

it("shows terminal failure without reviving old mean or automatically retrying", async () => {
  rows[0] = row(49, studyActualFixture(49, { status: "failed", stage: "failed", error: { code: "market_unavailable", stage: "acquiring", message: "История недоступна" } }));
  surface(); await settle(); expect(within(tableRow()).getByRole("alert")).toHaveTextContent("market_unavailable: История недоступна");
  expect(button()).toBeEnabled(); await settle(9000); expect(posts()).toHaveLength(0);
});

it("does not infer legacy eligibility from completion status or invent missing execution start", async () => {
  rows[0] = row(49, studyActualFixture(49, { eligibility: { eligible: false, reason: "execution_start_unavailable" } }));
  rows[1] = row(48, null); surface(); await settle();
  expect(screen.getByRole("button", { name: /исследования 49 недоступен: execution_start_unavailable/ })).toBeDisabled();
  expect(screen.getByRole("button", { name: /исследования 48 недоступен/ })).toBeDisabled(); expect(posts()).toHaveLength(0);
});

it("cleans up pending page polling and requests on unmount", async () => {
  rows[0] = row(49, studyActualFixture(49, { status: "queued", stage: "queued", revision: 1, jobId: "server-owned" }));
  const view = surface(); await settle(); const count = requests.length; view.unmount(); await settle(15000);
  expect(requests).toHaveLength(count); expect(requests.every((request) => request.signal?.aborted)).toBe(true);
});

describe("displayed-page request ownership", () => {
  const previous = () => screen.getByRole("button", { name: "Предыдущая страница исследований · Всплеск" });
  const next = () => screen.getByRole("button", { name: "Следующая страница исследований · Всплеск" });
  const table = () => screen.getByRole("table", { name: "Исследования · Всплеск" });
  const history = (snapshot = controller, routeActive = true, active = true) => <RouteActiveContext.Provider value={routeActive}>
    <OptimizationHistory algorithmId="spike" algorithmName="Всплеск" controller={snapshot} active={active} onSelect={vi.fn()} onDelete={vi.fn()} />
  </RouteActiveContext.Provider>;
  function deferPages() {
    const pending: { complete: (response: Response) => void; signal: AbortSignal | null | undefined }[] = [];
    readPage = () => new Promise<Response>((resolve) => { pending.push({ complete: resolve, signal: requests.at(-1)?.signal }); });
    return pending;
  }
  beforeEach(() => { rows = Array.from({ length: 60 }, (_, index) => row(60 - index)); });

  it("keeps both middle-page controls and the focused row usable throughout a deferred same-page actual refresh", async () => {
    rows[20] = row(40, studyActualFixture(40, { status: "queued", stage: "queued", revision: 1, jobId: "pending" }));
    render(history()); await settle(); fireEvent.click(next()); await settle();
    const trigger = screen.getByRole("button", { name: "Результат исследования 40" });
    act(() => { trigger.focus(); });
    const pending = deferPages(); await settle(3000);
    expect(pending).toHaveLength(1); expect(table()).toHaveAttribute("aria-busy", "true");
    expect(previous()).toBeEnabled(); expect(next()).toBeEnabled();
    expect(screen.getByRole("button", { name: "Результат исследования 40" })).toBe(trigger);
    expect(trigger).toHaveFocus(); expect(table().querySelector('[data-slot="skeleton"]')).toBeNull();
    rows[20] = row(40, studyActualFixture(40, { status: "completed", stage: "completed", revision: 1, jobId: "pending", counts: actualCountsFixture, meanR: 1.25 }));
    await act(async () => { pending[0]!.complete(pageResponse(20)); await Promise.resolve(); }); await settle();
    expect(within(tableRow(40)).getByText("+1.25R")).toBeInTheDocument();
    expect(table()).toHaveAttribute("aria-busy", "false"); expect(trigger).toHaveFocus();
    const count = requests.length; await settle(9000); expect(requests).toHaveLength(count);
  });

  it.each([0, 20])("serializes repeated discovery snapshots behind offset %s without aborting or starving accepted reads", async (offset) => {
    const latest = { ...row(61), status: "running" as const };
    let snapshot = { ...controller, latestByAlgorithm: { spike: latest } };
    const view = render(history(snapshot)); await settle();
    if (offset > 0) { fireEvent.click(next()); await settle(); }
    const count = requests.length;
    const pending = deferPages();
    for (let update = 1; update <= 5; update += 1) {
      snapshot = { ...snapshot, latestByAlgorithm: { spike: { ...latest,
        // An active first page also observes progress snapshots with the same updatedAt.
        updatedAt: offset === 0 ? latest.updatedAt : `2026-11-01T00:0${update + 1}:00Z`,
        progress: { ...latest.progress, elapsedSeconds: 60 + update },
      } } };
      view.rerender(history(snapshot)); await settle();
    }
    expect(requests).toHaveLength(count + 1); expect(pending).toHaveLength(1);
    expect(pending[0]!.signal?.aborted).toBe(false);
    rows[offset] = { ...rows[offset]!, bestScore: 0.91 };
    await act(async () => { pending[0]!.complete(pageResponse(offset)); await Promise.resolve(); }); await settle();
    expect(within(tableRow(60 - offset)).getByText("0.91 R")).toBeInTheDocument();
    // All intervening discovery updates coalesce to just one follow-up, not a queue.
    expect(pending).toHaveLength(2); expect(requests).toHaveLength(count + 2);
    expect(pending[1]!.signal?.aborted).toBe(false);
    rows[offset] = { ...rows[offset], bestScore: 1.25 };
    await act(async () => { pending[1]!.complete(pageResponse(offset)); await Promise.resolve(); }); await settle();
    expect(within(tableRow(60 - offset)).getByText("1.25 R")).toBeInTheDocument();
    expect(table()).toHaveAttribute("aria-busy", "false");
    await settle(9000); expect(requests).toHaveLength(count + 2);
  });

  it("blocks a real page transition with skeletons and rejects the old page's late refresh even if transport ignores abort", async () => {
    const view = render(history()); await settle(); fireEvent.click(next()); await settle();
    const pending = deferPages();
    view.rerender(history({ ...controller, revision: 1 })); await settle();
    expect(pending).toHaveLength(1);
    fireEvent.click(next()); await settle();
    expect(pending[0]!.signal?.aborted).toBe(true); expect(pending).toHaveLength(2);
    expect(table().querySelectorAll('[data-slot="skeleton"]')).toHaveLength(220);
    expect(table()).toHaveAttribute("aria-busy", "true");
    expect(previous()).toBeDisabled(); expect(next()).toBeDisabled();
    expect(screen.queryByRole("button", { name: "Результат исследования 40" })).not.toBeInTheDocument();
    await act(async () => { pending[1]!.complete(pageResponse(40)); await Promise.resolve(); }); await settle();
    expect(screen.getByText("Страница 3 · всего 60")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Результат исследования 20" })).toBeInTheDocument();
    expect(previous()).toBeEnabled(); expect(next()).toBeDisabled();
    rows[20] = { ...rows[20]!, bestScore: 9.99 };
    await act(async () => { pending[0]!.complete(pageResponse(20)); await Promise.resolve(); }); await settle();
    expect(screen.queryByRole("button", { name: "Результат исследования 40" })).not.toBeInTheDocument();
    expect(screen.queryByText("9.99 R")).not.toBeInTheDocument();
    expect(table()).toHaveAttribute("aria-busy", "false"); expect(pending).toHaveLength(2);
  });

  it.each(["route", "panel", "document", "unmount"])("cancels owned work and coalesced refresh on %s cleanup and fences late responses", async (boundary) => {
    const view = render(history()); await settle();
    const trigger = screen.getByRole("button", { name: "Результат исследования 60" });
    const pending = deferPages();
    view.rerender(history({ ...controller, revision: 1 })); await settle();
    view.rerender(history({ ...controller, revision: 2 })); await settle();
    if (boundary === "route") view.rerender(history(controller, false));
    else if (boundary === "panel") view.rerender(history(controller, true, false));
    else if (boundary === "document") {
      vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
      act(() => { document.dispatchEvent(new Event("visibilitychange")); });
    } else view.unmount();
    expect(pending[0]!.signal?.aborted).toBe(true);
    const count = requests.length;
    rows[0] = { ...rows[0]!, bestScore: 9.99 };
    await act(async () => { for (const request of pending) request.complete(pageResponse()); await Promise.resolve(); });
    await settle(60000); expect(requests).toHaveLength(count); expect(vi.getTimerCount()).toBe(0);
    if (boundary !== "unmount") {
      expect(screen.getByRole("button", { name: "Результат исследования 60" })).toBe(trigger);
      expect(within(tableRow(60)).queryByText("9.99 R")).not.toBeInTheDocument();
      readPage = (params) => pageResponse(Number(params.get("offset")));
      if (boundary === "document") {
        vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
        act(() => { document.dispatchEvent(new Event("visibilitychange")); });
      } else view.rerender(history());
      await settle(); expect(requests).toHaveLength(count + 1);
      expect(within(tableRow(60)).getByText("9.99 R")).toBeInTheDocument();
    }
    expect(posts()).toHaveLength(0);
  });
});

describe("real serialized compact-list producer parity", () => {
  type ProducerPhase = "idle" | "queued" | "running" | "completed" | "failed";
  // Captured cookie-authenticated native HTTP pages, not summarizeOptimizationStudy output.
  const pages = JSON.parse(producerPagesJson) as Record<ProducerPhase, { studies: { id: number; actual: StudyActualSummary }[] }>;
  let phase: ProducerPhase;
  beforeEach(() => {
    phase = "idle";
    vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
      const method = init?.method ?? "GET";
      requests.push({ path, method, body: init?.body, ...(init?.signal ? { signal: init.signal } : {}) });
      if (path.startsWith("/v1/optimization-studies?") && method === "GET") return Promise.resolve(json(pages[phase]));
      if (path === "/v1/optimization-studies/1/actual/refresh" && method === "POST") {
        phase = "queued"; return Promise.resolve(json(pages.queued.studies.find((study) => study.id === 1)!.actual, 202));
      }
      return Promise.resolve(json({ error: "unexpected request in captured-list parity" }, 404));
    }));
  });
  const legacy = () => screen.getByRole("button", { name: "Расчёт после исследования 2 недоступен: execution_start_unavailable" });
  const onlyCompactReads = () => expect(requests.filter((request) => request.method === "GET").every((request) => request.path.startsWith("/v1/optimization-studies?"))).toBe(true);

  it("makes the first manual click available from the real idle list alone and keeps the legacy row disabled", async () => {
    surface(false, 100); await settle();
    expect(button(1)).toBeEnabled(); expect(legacy()).toBeDisabled();
    expect(within(tableRow(1)).getByText("Не рассчитано")).toBeInTheDocument();
    expect(requests).toHaveLength(1); expect(posts()).toHaveLength(0); onlyCompactReads();
    fireEvent.click(legacy()); expect(posts()).toHaveLength(0);
    fireEvent.click(button(1)); fireEvent.click(button(1));
    expect(button(1)).toBeDisabled(); expect(posts()).toHaveLength(1); expect(posts()[0]?.body).toBe("{}");
    await settle();
    expect(within(tableRow(1)).getByRole("status")).toHaveTextContent("В очереди");
    expect(button(1)).toHaveAttribute("aria-busy", "true"); expect(legacy()).toBeDisabled();
    expect(posts()[0]?.path).toBe("/v1/optimization-studies/1/actual/refresh"); onlyCompactReads();
  });

  it.each(["queued", "running"] as const)("restores real %s state across reload and observes explicit completed counts using list polling only", async (pending) => {
    phase = pending;
    const view = surface(false, 100); await settle();
    expect(button(1)).toBeDisabled(); expect(button(1).querySelector("svg")).toHaveClass("motion-safe:animate-spin");
    const stage = pending === "queued" ? "В очереди" : "Расчёт исходов";
    expect(within(tableRow(1)).getByRole("status")).toHaveTextContent(stage);
    view.unmount(); surface(false, 100); await settle();
    expect(button(1)).toBeDisabled(); expect(within(tableRow(1)).getByRole("status")).toHaveTextContent(stage);
    phase = "completed"; await settle(3000);
    expect(button(1)).toBeEnabled(); expect(legacy()).toBeDisabled();
    expect(within(tableRow(1)).getByText("+0.00R")).toHaveClass("text-muted-foreground");
    const badge = within(tableRow(1)).getByText("2/2"); expect(badge).toHaveClass("text-ambient-violet");
    fireEvent.focus(badge);
    expect(screen.getByRole("tooltip")).toHaveTextContent("Закрытые сделки / все сетапы");
    expect(screen.getByRole("tooltip")).toHaveTextContent("TP: 1 · SL: 1 · гейт не пройден: 0 · не закрылись: 0 · нет данных: 0");
    expect(posts()).toHaveLength(0); onlyCompactReads();
    const count = requests.length; await settle(9000); expect(requests).toHaveLength(count);
  });

  it("renders the real failed revision and null counts without a fake zero, old badge or automatic retry", async () => {
    phase = "failed"; surface(false, 100); await settle();
    expect(within(tableRow(1)).getByRole("alert")).toHaveTextContent("fixture-failure: fixture-failure");
    expect(within(tableRow(1)).queryByText("+0.00R")).not.toBeInTheDocument();
    expect(within(tableRow(1)).queryByText("2/2")).not.toBeInTheDocument();
    expect(button(1)).toBeEnabled(); expect(legacy()).toBeDisabled();
    await settle(9000); expect(requests).toHaveLength(1); expect(posts()).toHaveLength(0); onlyCompactReads();
  });
});
