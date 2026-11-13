import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { SetupsPage } from "./setups-page";
import { RouteActiveContext } from "./route-active";
import type { SetupRow } from "./platform-controls";

const row = (id: number): SetupRow => ({ id, exchange: "bybit", category: "linear", symbol: `BTC${id}`, algorithmId: "price_volume_spike", openTimeUnix: 100, closeTimeUnix: 200, closePrice: "100" });
const requests: { path: string; signal: AbortSignal | null | undefined }[] = [];
let rows: SetupRow[];
let respond: (params: URLSearchParams) => Response | Promise<Response>;
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });
function page(params: URLSearchParams) {
  const snapshot = Number(params.get("snapshotId") ?? rows[0]?.id ?? 0);
  const filtered = rows.filter((item) => item.id! <= snapshot && item.symbol.includes(params.get("q") ?? ""));
  const offset = Number(params.get("offset"));
  return json({ setups: filtered.slice(offset, offset + 25), total: filtered.length, offset, limit: 25, snapshotId: snapshot });
}
const calls = () => requests.filter((request) => request.path.startsWith("/v1/setups?"));
const params = () => new URL(calls().at(-1)!.path, "http://localhost").searchParams;
const settle = async () => { await act(async () => { await vi.advanceTimersByTimeAsync(0); }); };
const tick = async (ms = 3000) => { await act(async () => { await vi.advanceTimersByTimeAsync(ms); }); };

beforeEach(() => {
  vi.useFakeTimers(); vi.setSystemTime(new Date("2026-11-13T12:00:00Z")); requests.length = 0;
  rows = Array.from({ length: 60 }, (_, index) => row(60 - index)); respond = page;
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    requests.push({ path, signal: init?.signal });
    if (path === "/v1/settings/ui") return Promise.resolve(json({ value: { setupFilter: "BTC" } }));
    return Promise.resolve(respond(new URL(path, "http://localhost").searchParams));
  }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("follows the fresh first page every three seconds, preserving the filter and deduplicating IDs", async () => {
  render(<SetupsPage />); await settle();
  expect(calls()).toHaveLength(1); expect(params().get("q")).toBe("BTC"); expect(params().has("snapshotId")).toBe(false);
  rows = [row(61), row(61), ...rows]; await tick();
  expect(calls()).toHaveLength(2); expect(screen.getAllByText("bybit · BTC61")).toHaveLength(1);
  expect(params().get("offset")).toBe("0"); expect(params().get("q")).toBe("BTC"); expect(params().has("snapshotId")).toBe(false);
  expect(screen.getByText(/^Последнее успешное чтение:/)).toBeInTheDocument();
});

it("pauses live refresh and explicitly resumes with a fresh snapshot", async () => {
  render(<SetupsPage />); await settle();
  fireEvent.click(screen.getByRole("button", { name: "Приостановить обновление" })); await settle();
  const pausedCalls = calls().length; rows.unshift(row(61)); await tick(9000);
  expect(calls()).toHaveLength(pausedCalls); expect(screen.queryByText("bybit · BTC61")).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Следить за live-лентой" })); await settle();
  expect(screen.getByText("bybit · BTC61")).toBeInTheDocument(); expect(params().has("snapshotId")).toBe(false);
  await tick(); expect(calls()).toHaveLength(pausedCalls + 2);
});

it("pins historical pages during background activity and preserves explicit Refresh as latest first page", async () => {
  render(<SetupsPage />); await settle();
  fireEvent.click(screen.getByRole("button", { name: "Далее" })); await settle();
  expect(params().get("offset")).toBe("25"); expect(params().get("snapshotId")).toBe("60");
  const historicalCalls = calls().length; rows.unshift(row(61)); await tick(9000);
  expect(calls()).toHaveLength(historicalCalls); expect(screen.getByText("bybit · BTC35")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Назад" })); await settle();
  expect(params().get("snapshotId")).toBe("60"); expect(screen.queryByText("bybit · BTC61")).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Следить за live-лентой" })); await settle();
  expect(params().get("offset")).toBe("0"); expect(params().has("snapshotId")).toBe(false); expect(screen.getByText("bybit · BTC61")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Далее" })); await settle();
  fireEvent.click(screen.getByRole("button", { name: "Обновить" })); await settle();
  expect(params().get("offset")).toBe("0"); expect(params().get("q")).toBe("BTC"); expect(params().has("snapshotId")).toBe(false);
  expect(screen.getByText("bybit · BTC61")).toBeInTheDocument();
});

it("retains the last confirmed rows and read time on transient failure and recovers automatically", async () => {
  render(<SetupsPage />); await settle();
  const lastRead = screen.getByText(/^Последнее успешное чтение:/).textContent;
  respond = () => json({}, 503); await tick();
  expect(screen.getByText("bybit · BTC60")).toBeInTheDocument();
  expect(screen.getByRole("alert")).toHaveTextContent("HTTP 503"); expect(screen.getByRole("alert")).toHaveTextContent("устареть");
  expect(screen.getByText(/^Последнее успешное чтение:/).textContent).toBe(lastRead);
  expect(screen.queryByText("Сетапы не найдены.")).not.toBeInTheDocument();
  respond = page; rows.unshift(row(61)); await tick();
  expect(screen.getByText("bybit · BTC61")).toBeInTheDocument(); expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(screen.getByText(/^Последнее успешное чтение:/).textContent).not.toBe(lastRead);
});

it("serializes slow reads, aborts inactive and hidden reads, and refreshes on activation without losing the filter", async () => {
  const view = render(<RouteActiveContext.Provider value><SetupsPage /></RouteActiveContext.Provider>); await settle();
  let finish: ((response: Response) => void) | undefined;
  respond = () => new Promise<Response>((resolve) => { finish = resolve; }); await tick();
  expect(calls()).toHaveLength(2); const slowSignal = calls().at(-1)?.signal;
  await tick(9000); expect(calls()).toHaveLength(2);
  view.rerender(<RouteActiveContext.Provider value={false}><SetupsPage /></RouteActiveContext.Provider>); await settle();
  expect(slowSignal?.aborted).toBe(true); await tick(9000); expect(calls()).toHaveLength(2);
  respond = page; rows.unshift(row(61));
  view.rerender(<RouteActiveContext.Provider value><SetupsPage /></RouteActiveContext.Provider>); await settle();
  expect(calls()).toHaveLength(3); expect(params().get("q")).toBe("BTC"); expect(screen.getByText("bybit · BTC61")).toBeInTheDocument();
  await act(async () => { finish?.(json({ setups: [], total: 0, offset: 0, limit: 25, snapshotId: 0 })); await Promise.resolve(); });
  expect(screen.getByText("bybit · BTC61")).toBeInTheDocument();
  const visibility = vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
  fireEvent(document, new Event("visibilitychange")); await tick(9000); expect(calls()).toHaveLength(3);
  expect(calls().at(-1)?.signal?.aborted).toBe(true);
  visibility.mockReturnValue("visible"); fireEvent(document, new Event("visibilitychange")); await settle();
  expect(calls()).toHaveLength(4); expect(params().has("snapshotId")).toBe(false);
  view.unmount(); expect(calls().at(-1)?.signal?.aborted).toBe(true); expect(vi.getTimerCount()).toBe(0);
  expect(requests.filter((request) => request.path === "/v1/settings/ui")).toHaveLength(1);
});

it("schedules the next refresh only after a slow response settles", async () => {
  render(<SetupsPage />); await settle();
  let finish: ((response: Response) => void) | undefined;
  respond = () => new Promise<Response>((resolve) => { finish = resolve; }); await tick(); await tick(9000);
  expect(calls()).toHaveLength(2);
  respond = page; await act(async () => { finish?.(page(new URLSearchParams({ offset: "0", q: "BTC" }))); await Promise.resolve(); });
  await tick(2999); expect(calls()).toHaveLength(2); await tick(1); expect(calls()).toHaveLength(3);
});

it("does not show a successful empty feed when the first read fails", async () => {
  respond = () => json({}, 503); render(<SetupsPage />); await settle();
  expect(screen.getByRole("alert")).toHaveTextContent("Лента недоступна");
  expect(screen.queryByText("Сетапы не найдены.")).not.toBeInTheDocument();
  respond = page; await tick(); expect(screen.getByText("bybit · BTC60")).toBeInTheDocument();
});
