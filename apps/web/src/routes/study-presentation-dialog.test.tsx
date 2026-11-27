import { StrictMode } from "react";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { studyActualFixture } from "../../test/study-actual-fixture";
import { applyInterfaceSettings, DEFAULT_INTERFACE_SETTINGS } from "../lib/datetime-format";
import type { OptimizationStudySummary } from "../lib/optimization-api";
import type { StudyPresentationSnapshot } from "../lib/study-presentation-api";
import { OptimizationHistory } from "./optimization-history";
import { RouteActiveContext } from "./route-active";
import type { OptimizationController } from "./use-optimization-studies";

const snapshot: StudyPresentationSnapshot = { revision: 7, source: "actual", sourceRevision: 3, observationCutoff: 1795201200,
  publishedAt: "2026-11-20T21:41:00Z", meanR: 0.25, counts: { closed: 4, tp: 2, sl: 2 } };
function row(id = 49): OptimizationStudySummary {
  return { id, origin: "continuous", status: "completed", spec: { algorithmId: "spike", algorithmVersion: "1", algorithmParams: {}, algorithmSearch: {},
    analyticsParams: { clusterEnabled: true, clusterGapMinutes: 480, timeShiftMinutes: 0, priceChangePct: null }, analyticsSearch: {}, instruments: [],
    fromUnix: 1794000000, toUnix: 1794086400, direction: "short", setupMaturity: 0, horizonMinutes: 10080, tpLevels: [4], slLevels: [4],
    minClosedPerCell: 1, seed: 1, maxTrials: 10, maxSeconds: 60 },
    progress: { phase: "done", evaluatedCandidates: 2, scorableCandidates: 2, maxTrials: 10, elapsedSeconds: 60, bestScore: 0.5 },
    bestScore: 0.5, error: null, actual: studyActualFixture(id), currentPresentation: { status: "available", snapshot },
    createdAt: "2026-11-01T00:00:00Z", updatedAt: "2026-11-01T00:01:00Z" };
}
const controller: OptimizationController = { byAlgorithm: {}, latestByAlgorithm: {}, active: null, ready: true, error: null, creating: false, cancelling: null, deleting: null, controlling: null, revision: 0,
  pause: vi.fn(() => Promise.resolve(true)), play: vi.fn(() => Promise.resolve(true)), start: vi.fn(() => Promise.resolve(true)), stop: vi.fn(() => Promise.resolve()),
  remove: vi.fn(() => Promise.resolve(true)), clearError: vi.fn(), refresh: vi.fn() };
let rows: OptimizationStudySummary[];
let png: () => Response | Promise<Response>;
const requests: { path: string; init: RequestInit }[] = [];
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });
const pngResponse = () => new Response(new Uint8Array([137, 80, 78, 71]), { headers: { "content-type": "image/png" } });
const pngRequests = () => requests.filter(({ path }) => path.includes("/presentation.png?"));
const trigger = (id = 49) => screen.getByRole("button", { name: `График PNG исследования ${id}` });
const createUrl = vi.fn<() => string>();
const revokeUrl = vi.fn<(url: string) => void>();
const settle = async (ms = 0) => { await act(async () => { await vi.advanceTimersByTimeAsync(ms); }); };
const onSelect = vi.fn();
function history(revision = 0, active = true, routeActive = true, pageSize = 20) {
  return <RouteActiveContext.Provider value={routeActive}><OptimizationHistory algorithmId="spike" algorithmName="Всплеск" controller={{ ...controller, revision }}
    active={active} onSelect={onSelect} onDelete={vi.fn()} pageSize={pageSize} /></RouteActiveContext.Provider>;
}
beforeEach(() => {
  vi.useFakeTimers(); rows = [row()]; requests.length = 0; onSelect.mockClear(); png = pngResponse;
  applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "UTC" });
  vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
  createUrl.mockReset().mockImplementation(() => `blob:study-${pngRequests().length}`); revokeUrl.mockReset();
  Object.defineProperty(URL, "createObjectURL", { configurable: true, writable: true, value: createUrl });
  Object.defineProperty(URL, "revokeObjectURL", { configurable: true, writable: true, value: revokeUrl });
  vi.stubGlobal("fetch", vi.fn((path: string, init: RequestInit = {}) => {
    requests.push({ path, init });
    if (path.startsWith("/v1/optimization-studies?")) {
      const params = new URL(path, "http://localhost").searchParams;
      const offset = Number(params.get("offset")); const limit = Number(params.get("limit"));
      return Promise.resolve(json({ studies: rows.slice(offset, offset + limit), total: rows.length, offset, limit }));
    }
    if (path.includes("/presentation.png?")) return Promise.resolve(png());
    return Promise.resolve(json({}, 404));
  }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); applyInterfaceSettings(DEFAULT_INTERFACE_SETTINGS); });

it("adds one dedicated compact column without fetching images/details or changing the ordinary study action, including StrictMode", async () => {
  render(<StrictMode>{history()}</StrictMode>); await settle();
  const table = screen.getByRole("table");
  expect(within(table).getAllByRole("columnheader")).toHaveLength(12);
  expect(within(table).getByRole("columnheader", { name: "График PNG" })).toBeInTheDocument();
  expect(table.parentElement).toHaveClass("overflow-x-auto", "min-h-[68.25rem]");
  expect(table.querySelectorAll("tbody tr")).toHaveLength(20);
  expect(table.querySelector('tr[aria-hidden="true"] td')).toHaveAttribute("colspan", "12");
  expect(trigger()).toBeEnabled(); expect(trigger()).toHaveClass("h-8");
  expect(pngRequests()).toHaveLength(0); expect(requests.every(({ path }) => path.startsWith("/v1/optimization-studies?"))).toBe(true);
  fireEvent.click(screen.getByRole("button", { name: "Результат исследования 49" }));
  expect(onSelect).toHaveBeenCalledWith(49, expect.any(String), true);
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument(); expect(pngRequests()).toHaveLength(0);
});

it.each(["queued", "running", "failed"] as const)("opens the previous complete PNG/statistics during a replacement job in state %s and restores focus on close", async (status) => {
  rows[0] = { ...row(), actual: studyActualFixture(49, { status, stage: status === "running" ? "evaluating" : status, revision: 4, jobId: "new-job", meanR: null, counts: null }) };
  render(history()); await settle();
  const button = trigger(); act(() => { button.focus(); }); fireEvent.click(button); await settle();
  const dialog = screen.getByRole("dialog", { name: "График PNG исследования #49" });
  expect(dialog).toHaveAccessibleDescription(expect.stringContaining("Просмотр не запускает расчёт"));
  const image = within(dialog).getByRole("img", { name: "Сохранённый график матожидания исследования 49, ревизия 7" });
  expect(image).toHaveAttribute("src", "blob:study-1"); expect(image).toHaveAttribute("width", "900"); expect(image).toHaveClass("w-full", "min-w-0");
  expect(within(dialog).getByText("7 / 3")).toBeInTheDocument(); expect(within(dialog).getByText("+0.25R")).toBeInTheDocument();
  expect(within(dialog).getByText("4 / 2 / 2")).toBeInTheDocument(); expect(within(dialog).getByText("20.11.2026, 19:00:00 (UTC)")).toBeInTheDocument();
  const request = pngRequests()[0]!;
  expect(request.path).toBe("/v1/optimization-studies/49/presentation.png?revision=7");
  expect(request.init.credentials).toBe("same-origin"); expect(new Headers(request.init.headers).get("x-platform-request")).toBe("1");
  expect(requests.every(({ init }) => (init.method ?? "GET") === "GET")).toBe(true);
  fireEvent.click(within(dialog).getByRole("button", { name: "Закрыть" })); await settle();
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument(); expect(button).toHaveFocus();
  expect(request.init.signal?.aborted).toBe(true); expect(revokeUrl).toHaveBeenCalledExactlyOnceWith("blob:study-1");
});

it("shows initial-only provenance and null statistics honestly, without requiring an actual job", async () => {
  rows[0] = { ...row(), actual: null, currentPresentation: { status: "available", snapshot: { ...snapshot, source: "initial", sourceRevision: null, meanR: null, counts: null } } };
  render(history()); await settle(); fireEvent.click(trigger()); await settle();
  const dialog = within(screen.getByRole("dialog"));
  expect(dialog.getByText("Исходные данные исследования")).toBeInTheDocument(); expect(dialog.getByText("7 / исходный снимок")).toBeInTheDocument();
  expect(dialog.getAllByText("Недоступно")).toHaveLength(2); expect(dialog.queryByText("+0.00R")).not.toBeInTheDocument();
});

it.each([undefined, null, { status: "unavailable", reason: "original_archive_missing" } as const])("makes missing/legacy presentation %j explicitly unavailable without any discovery or calculation", async (currentPresentation) => {
  const saved = row();
  if (currentPresentation === undefined) delete saved.currentPresentation;
  else saved.currentPresentation = currentPresentation;
  rows[0] = saved;
  render(history()); await settle();
  const disabled = screen.getByRole("button", { name: "График PNG исследования 49 недоступен" });
  expect(disabled).toBeDisabled(); expect(disabled).toHaveTextContent("Недоступен");
  expect(disabled).toHaveAccessibleDescription(currentPresentation?.status === "unavailable" ? "original_archive_missing" : "сохранённый снимок отсутствует");
  fireEvent.click(disabled); expect(pngRequests()).toHaveLength(0); expect(requests).toHaveLength(1);
});

it("rejects a stale revision visibly and reloads only the compact page before fetching matching new PNG/statistics", async () => {
  let stale = true;
  png = () => stale ? json({ error: "study_presentation_revision_changed" }, 409) : pngResponse();
  render(history()); await settle(); fireEvent.click(trigger()); await settle();
  expect(screen.getByRole("alert")).toHaveTextContent("PNG-снимок обновился"); expect(screen.queryByRole("img")).not.toBeInTheDocument();
  expect(createUrl).not.toHaveBeenCalled();
  rows[0] = { ...row(), currentPresentation: { status: "available", snapshot: { ...snapshot, revision: 8, sourceRevision: 4, meanR: -0.5 } } }; stale = false;
  fireEvent.click(screen.getByRole("button", { name: "Загрузить текущую версию PNG" })); await settle();
  expect(screen.getByRole("img")).toHaveAccessibleName("Сохранённый график матожидания исследования 49, ревизия 8");
  expect(within(screen.getByRole("dialog")).getByText("8 / 4")).toBeInTheDocument();
  expect(within(screen.getByRole("dialog")).getByText("-0.50R")).toBeInTheDocument();
  expect(pngRequests().map(({ path }) => path)).toEqual(["/v1/optimization-studies/49/presentation.png?revision=7", "/v1/optimization-studies/49/presentation.png?revision=8"]);
  expect(requests.filter(({ path }) => path.startsWith("/v1/optimization-studies?"))).toHaveLength(2);
  expect(requests.every(({ init }) => (init.method ?? "GET") === "GET")).toBe(true);
});

it.each([404, 500])("reports HTTP %s without a fake image or automatic retry, and retries only GET on request", async (status) => {
  png = () => json({ error: status === 404 ? "study_presentation_unavailable" : "internal" }, status);
  render(history()); await settle(); fireEvent.click(trigger()); await settle();
  expect(screen.getByRole("alert")).toHaveTextContent(status === 404 ? "Сохранённый PNG исследования недоступен" : "HTTP 500");
  expect(screen.queryByRole("img")).not.toBeInTheDocument(); await settle(9000); expect(pngRequests()).toHaveLength(1);
  png = pngResponse; fireEvent.click(screen.getByRole("button", { name: "Повторить загрузку PNG" })); await settle();
  expect(screen.getByRole("img")).toBeInTheDocument(); expect(pngRequests()).toHaveLength(2);
  expect(requests.every(({ init }) => (init.method ?? "GET") === "GET")).toBe(true);
});

it("fences an old pending image when the successful publication advances, and releases the sole URL when replaced", async () => {
  let finish: ((response: Response) => void) | undefined;
  png = () => new Promise<Response>((resolve) => { finish = resolve; });
  const view = render(history()); await settle(); fireEvent.click(trigger()); await settle();
  const first = pngRequests()[0]!;
  rows[0] = { ...row(), currentPresentation: { status: "available", snapshot: { ...snapshot, revision: 8 } } }; png = pngResponse;
  view.rerender(history(1)); await settle();
  expect(first.init.signal?.aborted).toBe(true); expect(screen.getByRole("img")).toHaveAccessibleName(expect.stringContaining("ревизия 8"));
  await act(async () => { finish?.(pngResponse()); await Promise.resolve(); }); await settle();
  expect(createUrl).toHaveBeenCalledOnce();
  rows[0] = { ...row(), currentPresentation: { status: "available", snapshot: { ...snapshot, revision: 9 } } };
  view.rerender(history(2)); await settle();
  expect(screen.getByRole("img")).toHaveAccessibleName(expect.stringContaining("ревизия 9"));
  expect(revokeUrl).toHaveBeenCalledExactlyOnceWith("blob:study-2");
  view.unmount(); expect(revokeUrl).toHaveBeenCalledTimes(2);
});

it.each(["close", "route", "panel", "unmount"])("aborts on %s and never allocates URLs for a late response even when transport ignores abort", async (boundary) => {
  let finish: ((response: Response) => void) | undefined;
  png = () => new Promise<Response>((resolve) => { finish = resolve; });
  const view = render(history()); await settle(); fireEvent.click(trigger()); await settle();
  expect(screen.getByRole("status", { name: "Загрузка сохранённого PNG исследования" })).toBeInTheDocument();
  const request = pngRequests()[0]!;
  if (boundary === "close") { fireEvent.click(screen.getByRole("button", { name: "Закрыть" })); await settle(); }
  else if (boundary === "route") view.rerender(history(0, true, false));
  else if (boundary === "panel") view.rerender(history(0, false));
  else view.unmount();
  expect(request.init.signal?.aborted).toBe(true);
  await act(async () => { finish?.(pngResponse()); await Promise.resolve(); }); await settle();
  expect(createUrl).not.toHaveBeenCalled(); expect(screen.queryByRole("img")).not.toBeInTheDocument();
});

it("releases the image while the document is hidden, reloads on visibility, and handles decoding failures explicitly", async () => {
  render(history()); await settle(); fireEvent.click(trigger()); await settle();
  vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
  act(() => { document.dispatchEvent(new Event("visibilitychange")); }); await settle();
  expect(revokeUrl).toHaveBeenCalledExactlyOnceWith("blob:study-1"); expect(screen.queryByRole("img")).not.toBeInTheDocument();
  vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
  act(() => { document.dispatchEvent(new Event("visibilitychange")); }); await settle();
  expect(pngRequests()).toHaveLength(2); fireEvent.error(screen.getByRole("img"));
  expect(screen.getByRole("alert")).toHaveTextContent("Не удалось прочитать сохранённое PNG-изображение");
  expect(revokeUrl).toHaveBeenCalledTimes(2); expect(screen.queryByRole("img")).not.toBeInTheDocument();
});

it("restores history-region focus if a refreshed row loses its PNG and removes the dialog", async () => {
  const view = render(history()); await settle(); fireEvent.click(trigger()); await settle();
  rows[0] = { ...row(), currentPresentation: { status: "unavailable", reason: "removed" } };
  view.rerender(history(1)); await settle();
  // Page acceptance unmounts the dialog at the end of React's async act;
  // Radix restores focus in its following zero-delay cleanup callback.
  await settle();
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(screen.getByRole("region", { name: "История оптимизации · Всплеск" })).toHaveFocus();
  expect(revokeUrl).toHaveBeenCalledOnce();
});

it("keeps metadata dates reactive without refetching server PNG or changing its UTC chart", async () => {
  render(history()); await settle(); fireEvent.click(trigger()); await settle();
  const count = pngRequests().length;
  act(() => { applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "Asia/Kathmandu", dateOrder: "ymd", dateSeparator: "-" }); });
  expect(within(screen.getByRole("dialog")).getByText("2026-11-21, 00:45:00 (Asia/Kathmandu)")).toBeInTheDocument();
  expect(pngRequests()).toHaveLength(count); expect(screen.getByRole("img")).toHaveAttribute("src", "blob:study-1");
});
