import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { SetupHistoryTable } from "./setup-history-table";
import { RouteActiveContext } from "./route-active";
import { applyInterfaceSettings, DEFAULT_INTERFACE_SETTINGS } from "../lib/datetime-format";
import type { SetupHistoryFilter } from "../lib/setups-analytics-api";

const filter: SetupHistoryFilter = { algorithmId: "price_volume_spike", fromUnix: 0, toUnix: 1000, q: "", studies: null, hideGateFailed: true };
const row = (id: number) => ({ id, symbol: `WATCH${id}`, exchange: "bybit", algorithmId: "price_volume_spike", openTimeUnix: 120, closeTimeUnix: 180, closePrice: "100", recommendation: null, tradeOutcome: null, params: { saved: 12 }, details: null });
const json = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status });
const page = (id: number, offset = 0) => json({ setups: [row(id)], total: 60, offset, limit: 25, snapshotId: 90 });
const calls: { path: string; signal: AbortSignal | null | undefined }[] = [];
let history: (params: URLSearchParams) => Response | Promise<Response>;
const canonical = "🔴 COIN😀\nАлгоритм: <script>not HTML</script>";
beforeEach(() => {
  vi.useFakeTimers(); calls.length = 0;
  applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "UTC" });
  history = (params) => page(Number(params.get("offset")) === 0 ? 1 : 2, Number(params.get("offset")));
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    calls.push({ path, signal: init?.signal });
    if (path.includes("/details")) return Promise.resolve(json({ setup: row(Number(path.split("/")[3])), telegramMessage: { text: canonical, entities: [{ type: "code", offset: 3, length: 6 }, { type: "bold", offset: 10, length: 8 }] } }));
    return Promise.resolve(history(new URL(path, "http://fixture").searchParams));
  }));
});
afterEach(() => { cleanup(); applyInterfaceSettings(DEFAULT_INTERFACE_SETTINGS); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });
const settle = async () => { await act(async () => { await vi.advanceTimersByTimeAsync(0); }); };

it("keeps WATCH/null outcomes visible and pins paging and route-return reads to one snapshot", async () => {
  const view = render(<RouteActiveContext.Provider value><SetupHistoryTable filter={filter} /></RouteActiveContext.Provider>);
  await settle();
  expect(screen.getByText("WATCH1")).toBeInTheDocument(); expect(screen.getByText("Неприменимо")).toHaveAttribute("data-slot", "badge");
  expect(screen.queryByText("+0.00R")).toBeNull();
  fireEvent.click(screen.getByRole("button", { name: "Далее" })); await settle();
  expect(screen.getByText("WATCH2")).toBeInTheDocument(); expect(screen.queryByText("WATCH1")).toBeNull();
  expect(new URL(calls.at(-1)!.path, "http://fixture").searchParams.get("snapshotId")).toBe("90");
  view.rerender(<RouteActiveContext.Provider value={false}><SetupHistoryTable filter={filter} /></RouteActiveContext.Provider>);
  view.rerender(<RouteActiveContext.Provider value><SetupHistoryTable filter={filter} /></RouteActiveContext.Provider>); await settle();
  const params = new URL(calls.at(-1)!.path, "http://fixture").searchParams;
  expect(params.get("snapshotId")).toBe("90"); expect(params.get("offset")).toBe("25");
});

it("aborts stale queries and retains last confirmed rows on an operational error", async () => {
  let finish: ((response: Response) => void) | undefined;
  history = () => new Promise<Response>((resolve) => { finish = resolve; });
  const view = render(<SetupHistoryTable filter={filter} />); await settle();
  const oldSignal = calls[0]!.signal;
  history = () => page(2);
  view.rerender(<SetupHistoryTable filter={{ ...filter, q: "new" }} />); await settle();
  expect(oldSignal?.aborted).toBe(true); expect(screen.getByText("WATCH2")).toBeInTheDocument();
  act(() => { finish!(page(1)); }); await settle();
  expect(screen.queryByText("WATCH1")).toBeNull();
  history = () => json({ error: "unavailable" }, 503);
  view.rerender(<SetupHistoryTable filter={{ ...filter, q: "broken" }} />); await settle();
  expect(screen.getByRole("alert")).toHaveTextContent("последний подтверждённый");
  expect(screen.getByText("WATCH2")).toBeInTheDocument(); expect(screen.queryByText("Сетапы не найдены.")).toBeNull();
});

it("renders canonical text/entities safely above saved details, preserves the modal snapshot and restores fallback focus", async () => {
  const view = render(<SetupHistoryTable filter={filter} />); await settle();
  const trigger = screen.getByRole("button", { name: "Подробности WATCH1" }); trigger.focus(); fireEvent.click(trigger); await settle();
  const dialog = screen.getByRole("dialog");
  expect(dialog.querySelector("pre")?.textContent).toBe(canonical);
  expect(dialog.querySelector("code")?.textContent).toBe("COIN😀");
  expect(dialog.querySelector("strong")?.textContent).toBe("Алгоритм");
  expect(dialog.querySelector("script")).toBeNull();
  expect(within(dialog).getByText("saved")).toBeInTheDocument(); expect(within(dialog).getByText("12")).toBeInTheDocument();
  history = () => page(2);
  view.rerender(<SetupHistoryTable filter={{ ...filter, q: "other" }} />); await settle();
  expect(dialog.querySelector("pre")?.textContent).toBe(canonical);
  fireEvent.click(within(dialog).getByRole("button", { name: "Закрыть" })); await settle();
  expect(screen.queryByRole("dialog")).toBeNull();
  expect(screen.getByRole("region", { name: "История сохранённых сетапов" })).toHaveFocus();
  expect(calls.every((call) => call.path.startsWith("/v1/setups?") || call.path.endsWith("/details"))).toBe(true);
});
