import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { ContinuousMaintenanceStatus } from "../lib/continuous-maintenance-api";
import { ContinuousMaintenance } from "./continuous-maintenance";
import { RouteActiveContext } from "./route-active";

const path = "/v1/continuous-studies/maintenance";
const running: ContinuousMaintenanceStatus = { admissionPaused: false, state: "running", activeChains: 0, resumableChains: 0, updatedAt: null };
const draining: ContinuousMaintenanceStatus = { admissionPaused: true, state: "draining", activeChains: 1, resumableChains: 1, updatedAt: "2026-11-14T12:00:00Z" };
const drained: ContinuousMaintenanceStatus = { ...draining, state: "drained", activeChains: 0, resumableChains: 0 };
let next: ContinuousMaintenanceStatus;
let readStatus: number;
let commandStatus: number;
const calls: { path: string; method: string; signal: AbortSignal | null | undefined }[] = [];
const json = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status });
const requestPath = (url: RequestInfo | URL) => typeof url === "string" ? url : url instanceof URL ? url.href : url.url;
const tick = async (ms = 0) => { await act(async () => { await vi.advanceTimersByTimeAsync(ms); }); };
const gets = () => calls.filter((call) => call.method === "GET");
const posts = () => calls.filter((call) => call.method === "POST");
const drainButton = () => screen.getByRole("button", { name: "Подготовить непрерывные исследования к обслуживанию" });
const resumeButton = () => screen.getByRole("button", { name: "Возобновить допуск непрерывных исследований" });
const refreshButton = () => screen.getByRole("button", { name: "Обновить статус обслуживания" });

beforeEach(() => {
  vi.useFakeTimers(); calls.length = 0; next = running; readStatus = 200; commandStatus = 200;
  vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
  vi.stubGlobal("fetch", vi.fn((url: string, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    calls.push({ path: url, method, signal: init?.signal });
    if (method === "POST") {
      if (commandStatus !== 200) return Promise.resolve(json({}, commandStatus));
      const paused = url.endsWith("/drain");
      next = { ...next, admissionPaused: paused, state: paused ? next.activeChains > 0 ? "draining" : "drained" : "running", updatedAt: "2026-11-14T12:00:00Z" };
      return Promise.resolve(json(next));
    }
    return Promise.resolve(json(next, readStatus));
  }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("shows only counts and drain/resume admission controls, allows resume while draining, and never sends Stop/Start", async () => {
  next = { ...running, activeChains: 1, resumableChains: 1 };
  render(<ContinuousMaintenance active />); await tick();
  expect(screen.getByText("Незавершённых цепочек: 1")).toBeInTheDocument();
  expect(screen.getByText("Цепочек с сохранённым снимком: 1")).toBeInTheDocument();
  expect(screen.getByText(/не гарантия совместимости будущей версии/)).toBeInTheDocument();
  expect(screen.getByText(/Это не Stop/)).toHaveTextContent("действующие адаптивные профили не отключаются");
  expect(resumeButton()).toBeDisabled();
  fireEvent.click(drainButton()); fireEvent.click(drainButton()); await tick();
  expect(screen.getByText("Допуск закрыт · ожидаем полного завершения цепочек")).toBeInTheDocument();
  expect(drainButton()).toBeDisabled(); expect(resumeButton()).toBeEnabled();
  fireEvent.click(resumeButton()); await tick();
  expect(screen.getByText("Допуск новых непрерывных цепочек открыт")).toBeInTheDocument();
  expect(posts().map((call) => call.path)).toEqual([`${path}/drain`, `${path}/resume`]);
  expect(gets()).toHaveLength(3); // Initial read, then authoritative reads after both commands.
  expect(calls.every((call) => call.path.startsWith(path))).toBe(true);
  expect(screen.queryByRole("table")).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: /деплой|обновить сервер/i })).not.toBeInTheDocument();
});

it("uses settled-request 15s idle and 3s active/draining cadence, returning to 15s when drained", async () => {
  const view = render(<ContinuousMaintenance active />); await tick();
  await tick(14999); expect(gets()).toHaveLength(1);
  await tick(1); expect(gets()).toHaveLength(2);
  next = draining;
  await tick(15000); expect(gets()).toHaveLength(3);
  await tick(2999); expect(gets()).toHaveLength(3);
  await tick(1); expect(gets()).toHaveLength(4);
  next = drained;
  await tick(3000); expect(gets()).toHaveLength(5);
  await tick(14999); expect(gets()).toHaveLength(5);
  await tick(1); expect(gets()).toHaveLength(6);
  view.unmount(); expect(vi.getTimerCount()).toBe(0);
});

it("observes active chains every 3s even when admission is running rather than draining", async () => {
  next = { ...running, activeChains: 1 };
  render(<ContinuousMaintenance active />); await tick();
  await tick(3000); expect(gets()).toHaveLength(2);
});

it("does not read hidden subsections/routes/documents and refreshes immediately on return", async () => {
  const visibility = vi.spyOn(document, "visibilityState", "get");
  const view = render(<RouteActiveContext.Provider value={true}><ContinuousMaintenance active={false} /></RouteActiveContext.Provider>);
  await tick(60000); expect(gets()).toHaveLength(0); expect(vi.getTimerCount()).toBe(0);
  view.rerender(<RouteActiveContext.Provider value={true}><ContinuousMaintenance active /></RouteActiveContext.Provider>);
  await tick(); expect(gets()).toHaveLength(1);
  view.rerender(<RouteActiveContext.Provider value={true}><ContinuousMaintenance active={false} /></RouteActiveContext.Provider>);
  await tick(60000); expect(gets()).toHaveLength(1); expect(vi.getTimerCount()).toBe(0);
  view.rerender(<RouteActiveContext.Provider value={false}><ContinuousMaintenance active /></RouteActiveContext.Provider>);
  await tick(60000); expect(gets()).toHaveLength(1);
  view.rerender(<RouteActiveContext.Provider value={true}><ContinuousMaintenance active /></RouteActiveContext.Provider>);
  await tick(); expect(gets()).toHaveLength(2);
  visibility.mockReturnValue("hidden"); act(() => { document.dispatchEvent(new Event("visibilitychange")); });
  await tick(60000); expect(gets()).toHaveLength(2); expect(vi.getTimerCount()).toBe(0);
  visibility.mockReturnValue("visible"); act(() => { document.dispatchEvent(new Event("visibilitychange")); });
  await tick(); expect(gets()).toHaveLength(3);
  view.unmount(); expect(vi.getTimerCount()).toBe(0);
});

it("never overlaps a slow read with polling, refresh or commands and starts cadence only after settlement", async () => {
  let resolveSlow: ((response: Response) => void) | undefined;
  vi.mocked(globalThis.fetch).mockImplementationOnce((url, init) => {
    calls.push({ path: requestPath(url), method: "GET", signal: init?.signal });
    return new Promise<Response>((resolve) => { resolveSlow = resolve; });
  });
  render(<ContinuousMaintenance active />); await tick(45000);
  expect(gets()).toHaveLength(1); expect(vi.getTimerCount()).toBe(0);
  expect(drainButton()).toBeDisabled(); expect(refreshButton()).toBeDisabled();
  fireEvent.click(drainButton()); fireEvent.click(refreshButton()); expect(calls).toHaveLength(1);
  await act(async () => { resolveSlow?.(json(running)); await Promise.resolve(); }); await tick();
  await tick(14999); expect(gets()).toHaveLength(1);
  await tick(1); expect(gets()).toHaveLength(2);
});

it("preserves the stale snapshot and disabled commands after a 5xx, then automatically recovers after 15s", async () => {
  next = draining;
  render(<ContinuousMaintenance active />); await tick();
  readStatus = 500; await tick(3000);
  expect(screen.getByText(/актуальность статуса обслуживания не подтверждена/)).toBeInTheDocument();
  expect(screen.getByText("Незавершённых цепочек: 1")).toBeInTheDocument();
  expect(screen.getByRole("alert")).toHaveTextContent("HTTP 500");
  expect(resumeButton()).toBeDisabled(); expect(drainButton()).toBeDisabled();
  await tick(14999); expect(gets()).toHaveLength(2);
  readStatus = 200; next = drained; await tick(1);
  expect(gets()).toHaveLength(3);
  expect(screen.getByText("Незавершённых цепочек: 0")).toBeInTheDocument();
  expect(screen.queryByText(/актуальность статуса обслуживания не подтверждена/)).not.toBeInTheDocument();
  expect(resumeButton()).toBeEnabled();
});

it.each(["network", "invalid DTO"])("automatically recovers from a transient %s failure without overlapping reads or retrying commands", async (failure) => {
  next = draining;
  render(<ContinuousMaintenance active />); await tick();
  vi.mocked(globalThis.fetch).mockImplementationOnce((url, init) => {
    calls.push({ path: requestPath(url), method: "GET", signal: init?.signal });
    return failure === "network" ? Promise.reject(new TypeError("Failed to fetch")) : Promise.resolve(json({}));
  });
  await tick(3000);
  expect(gets()).toHaveLength(2); expect(resumeButton()).toBeDisabled();
  expect(screen.getByText(/актуальность статуса обслуживания не подтверждена/)).toBeInTheDocument();
  next = drained;
  await tick(14999); expect(gets()).toHaveLength(2);
  await tick(1); expect(gets()).toHaveLength(3);
  expect(resumeButton()).toBeEnabled(); expect(posts()).toHaveLength(0);
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
});

it.each([404, 405, 501])("reports old-server HTTP %i as unsupported, never drained, without error polling or automatic mutations", async (status) => {
  readStatus = status;
  render(<ContinuousMaintenance active />); await tick();
  expect(screen.getByRole("alert")).toHaveTextContent("не поддерживается этой версией сервера");
  expect(screen.getByText("Статус обслуживания неизвестен.")).toBeInTheDocument();
  expect(screen.queryByText("Допуск закрыт · непрерывные цепочки завершены")).not.toBeInTheDocument();
  expect(resumeButton()).toBeDisabled(); expect(drainButton()).toBeDisabled();
  await tick(60000); expect(gets()).toHaveLength(1); expect(posts()).toHaveLength(0);
  readStatus = 200; fireEvent.click(refreshButton()); await tick();
  expect(drainButton()).toBeEnabled();
});

it.each([401, 403])("waits for explicit refresh or return after authorization HTTP %i", async (status) => {
  readStatus = status;
  render(<ContinuousMaintenance active />); await tick(60000);
  expect(gets()).toHaveLength(1); expect(posts()).toHaveLength(0);
  expect(drainButton()).toBeDisabled(); expect(resumeButton()).toBeDisabled();
  readStatus = 200; fireEvent.click(refreshButton()); await tick();
  expect(drainButton()).toBeEnabled();
});

it("reconciles a failed mutation through GET without retrying the command or hiding its error", async () => {
  render(<ContinuousMaintenance active />); await tick();
  commandStatus = 500; fireEvent.click(drainButton()); await tick();
  expect(posts()).toHaveLength(1); expect(gets()).toHaveLength(2);
  expect(screen.getByRole("alert")).toHaveTextContent("Команда не подтверждена");
  expect(screen.getByText("Допуск новых непрерывных цепочек открыт")).toBeInTheDocument();
  await tick(15000); expect(posts()).toHaveLength(1);
});

it("discards an aborted hidden-subsection response and releases pending reads on unmount", async () => {
  let resolveSlow: ((response: Response) => void) | undefined;
  vi.mocked(globalThis.fetch).mockImplementationOnce((url, init) => {
    calls.push({ path: requestPath(url), method: "GET", signal: init?.signal });
    return new Promise<Response>((resolve) => { resolveSlow = resolve; });
  });
  const view = render(<ContinuousMaintenance active />); await tick();
  view.rerender(<ContinuousMaintenance active={false} />);
  expect(gets()[0]?.signal?.aborted).toBe(true);
  view.rerender(<ContinuousMaintenance active />); await tick();
  await act(async () => { resolveSlow?.(json(drained)); await Promise.resolve(); }); await tick();
  expect(screen.getByText("Допуск новых непрерывных цепочек открыт")).toBeInTheDocument();
  expect(screen.queryByText("Допуск закрыт · непрерывные цепочки завершены")).not.toBeInTheDocument();
  view.unmount(); expect(vi.getTimerCount()).toBe(0);
});

it("finishes an explicit command while the document is hidden but waits until visible to reconcile its status", async () => {
  const visibility = vi.spyOn(document, "visibilityState", "get");
  render(<ContinuousMaintenance active />); await tick();
  let resolveCommand: ((response: Response) => void) | undefined;
  vi.mocked(globalThis.fetch).mockImplementationOnce((url, init) => {
    calls.push({ path: requestPath(url), method: "POST", signal: init?.signal });
    return new Promise<Response>((resolve) => { resolveCommand = resolve; });
  });
  fireEvent.click(drainButton()); await tick();
  visibility.mockReturnValue("hidden"); act(() => { document.dispatchEvent(new Event("visibilitychange")); });
  expect(posts()[0]?.signal?.aborted).toBe(false);
  next = drained; await act(async () => { resolveCommand?.(json(drained)); await Promise.resolve(); }); await tick(60000);
  expect(gets()).toHaveLength(1); expect(posts()).toHaveLength(1);
  visibility.mockReturnValue("visible"); act(() => { document.dispatchEvent(new Event("visibilitychange")); });
  await tick(); expect(gets()).toHaveLength(2); expect(resumeButton()).toBeEnabled();
});
