import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { afterEach, expect, it, vi } from "vitest";
import { RunsPage } from "./runs-page";
import { ReplayPreflight, type PreflightResponse } from "./replay-preflight";

afterEach(() => { cleanup(); vi.unstubAllGlobals(); });
const identity = { exchange: "bybit", category: "linear", symbol: "BTCUSDT" };
const preview: PreflightResponse = {
  checkedAt: "2026-11-01T00:00:00Z", algorithmVersion: "fixed-v2", requiredHistoryMinutes: 43200, canRun: true,
  request: { instruments: [identity], fromUnix: 100, toUnix: 200, algorithmId: "price_volume_spike", algorithmVersion: "fixed-v2", params: { priceGrowthPct: "19", baselineLongDays: 30 }, async: true },
  results: [{ ...identity, cacheHit: true, status: "cached", computedAt: "2026-10-01T00:00:00Z", quality: null, setupCount: 0 }],
};
function mockApi(options: { offline?: boolean; legacy?: boolean; check?: (signal: AbortSignal | null | undefined) => Promise<Response>; launchStatus?: number } = {}) {
  const writes: { path: string; body: unknown }[] = [];
  const fetcher = vi.fn((path: string, init?: RequestInit) => {
    if (init?.method === "POST") writes.push({ path, body: JSON.parse(typeof init.body === "string" ? init.body : "{}") as unknown });
    if (path === "/v1/replay-preflight") return options.check ? options.check(init?.signal) : Promise.resolve(new Response(JSON.stringify(preview)));
    if (path === "/v1/replay-runs" && init?.method === "POST") return Promise.resolve(new Response("{}", { status: options.launchStatus ?? 200 }));
    if (path === "/v1/algorithms") return Promise.resolve(new Response(JSON.stringify({ algorithms: [{ id: "price_volume_spike", name: "Алгоритм", version: "current-v1" }] })));
    if (path === "/v1/instruments") return Promise.resolve(new Response(JSON.stringify({ instruments: [{ ...identity, id: 1, status: "active" }] }), { status: options.offline ? 503 : 200 }));
    if (path === "/v1/settings/ui") return Promise.resolve(new Response(JSON.stringify({ value: { replay: { symbols: "BTCUSDT", algorithmId: "price_volume_spike", from: "2026-11-01T12:00", to: "2026-11-01T13:00", ...(options.legacy ? {} : { instruments: [identity] }) } } })));
    return Promise.resolve(new Response(JSON.stringify({ runs: [] })));
  });
  vi.stubGlobal("fetch", fetcher);
  return { writes, fetcher };
}
async function openForm() {
  render(<MemoryRouter><RunsPage /></MemoryRouter>);
  await waitFor(() => expect(screen.getByRole("button", { name: "Проверить данные" })).toBeEnabled());
}
it("checks cache offline using persisted identities and launches only the exact confirmed snapshot", async () => {
  const { writes } = mockApi({ offline: true });
  await openForm();
  await userEvent.click(screen.getByRole("button", { name: "Проверить данные" }));
  await screen.findByText(/Качество: Качество не сохранено/);
  expect(writes.map((item) => item.path)).toEqual(["/v1/replay-preflight"]);
  expect(writes[0]?.body).toMatchObject({ instruments: [identity] });
  expect(screen.queryByText(/Прогрев:/)).not.toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "Запустить проверенный расчёт" }));
  await screen.findByText(/Запрос запуска принят/);
  expect(writes[1]).toEqual({ path: "/v1/replay-runs", body: preview.request });
});
it("does not invent an offline identity from a legacy symbol-only preference", async () => {
  const { writes } = mockApi({ offline: true, legacy: true });
  await openForm(); await userEvent.click(screen.getByRole("button", { name: "Проверить данные" }));
  await screen.findByText(/По одним символам offline-проверка невозможна/);
  expect(writes).toHaveLength(0);
});
it("saves lowercase symbols with full identities and reopens them for offline cache preflight", async () => {
  const { writes } = mockApi();
  const original = globalThis.fetch;
  let offline = false;
  let saved: Record<string, unknown> = { symbols: " btcusdt, btcusdt ", algorithmId: "price_volume_spike", from: "2026-11-01T12:00", to: "2026-11-01T13:00" };
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    if (path === "/v1/settings/ui") {
      if (init?.method === "PUT" && typeof init.body === "string") saved = (JSON.parse(init.body) as { value: { replay: Record<string, unknown> } }).value.replay;
      return Promise.resolve(new Response(JSON.stringify({ value: { replay: saved } })));
    }
    if (offline && path === "/v1/instruments") return Promise.resolve(new Response("{}", { status: 503 }));
    return original(path, init);
  }));
  const first = render(<MemoryRouter><RunsPage /></MemoryRouter>);
  await waitFor(() => expect(screen.getByRole("button", { name: "Сохранить форму" })).toBeEnabled());
  await userEvent.click(screen.getByRole("button", { name: "Сохранить форму" }));
  await screen.findByText("Форма сохранена в базе сигналов.");
  expect(saved).toMatchObject({ symbols: "BTCUSDT", instruments: [identity] });
  first.unmount(); offline = true;
  await openForm();
  await userEvent.click(screen.getByRole("button", { name: "Проверить данные" }));
  await screen.findByText(/Качество: Качество не сохранено/);
  expect(writes.find((item) => item.path === "/v1/replay-preflight")?.body).toMatchObject({ instruments: [identity] });
  expect(writes.some((item) => item.path === "/v1/replay-runs")).toBe(false);
});
it("aborts preflight on edits and ignores an aborted late response", async () => {
  let resolveLate: ((response: Response) => void) | undefined;
  let signal: AbortSignal | null | undefined;
  const { writes } = mockApi({ check: (value) => { signal = value; return new Promise((resolve) => { resolveLate = resolve; }); } });
  await openForm(); await userEvent.click(screen.getByRole("button", { name: "Проверить данные" }));
  await waitFor(() => expect(resolveLate).toBeDefined());
  fireEvent.change(screen.getByLabelText("Конец"), { target: { value: "2026-11-01T14:00" } });
  expect(signal?.aborted).toBe(true);
  await act(async () => { resolveLate?.(new Response(JSON.stringify(preview))); await Promise.resolve(); });
  expect(screen.queryByRole("button", { name: "Запустить проверенный расчёт" })).not.toBeInTheDocument();
  expect(writes).toHaveLength(1);
});
it("invalidates a ready preview on selection change", async () => {
  mockApi(); await openForm(); await userEvent.click(screen.getByRole("button", { name: "Проверить данные" }));
  await screen.findByRole("button", { name: "Запустить проверенный расчёт" });
  await userEvent.click(screen.getByRole("button", { name: "Убрать все" }));
  expect(screen.queryByRole("button", { name: "Запустить проверенный расчёт" })).not.toBeInTheDocument();
});
it("aborts an in-flight preflight on unmount", async () => {
  let signal: AbortSignal | null | undefined;
  mockApi({ check: (value) => { signal = value; return new Promise(() => undefined); } });
  const view = render(<MemoryRouter><RunsPage /></MemoryRouter>);
  await waitFor(() => expect(screen.getByRole("button", { name: "Проверить данные" })).toBeEnabled());
  await userEvent.click(screen.getByRole("button", { name: "Проверить данные" }));
  await waitFor(() => expect(signal).toBeDefined()); view.unmount();
  expect(signal?.aborted).toBe(true);
});
it("shows an HTTP preflight failure without restoring confirmation", async () => {
  const { writes } = mockApi({ check: () => Promise.resolve(new Response("{}", { status: 502 })) });
  await openForm(); await userEvent.click(screen.getByRole("button", { name: "Проверить данные" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("Проверка данных недоступна: HTTP 502");
  expect(screen.queryByRole("button", { name: "Запустить проверенный расчёт" })).not.toBeInTheDocument();
  expect(writes).toHaveLength(1);
});
it("keeps honest preview when launch fails and blocks canRun false", async () => {
  mockApi({ launchStatus: 409 }); await openForm(); await userEvent.click(screen.getByRole("button", { name: "Проверить данные" }));
  await userEvent.click(await screen.findByRole("button", { name: "Запустить проверенный расчёт" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("HTTP 409");
  expect(screen.getByRole("heading", { name: "Предварительная проверка данных" })).toBeVisible();
});
it.each(["unavailable", "not_found"] as const)("blocks confirmation for %s without creating jobs", async (status) => {
  const { writes } = mockApi({ check: () => Promise.resolve(new Response(JSON.stringify({ ...preview, canRun: false, results: [{ ...identity, cacheHit: false, status, error: "upstream unavailable" }] }))) });
  await openForm(); await userEvent.click(screen.getByRole("button", { name: "Проверить данные" }));
  expect(await screen.findByRole("button", { name: "Запустить проверенный расчёт" })).toBeDisabled();
  expect(screen.queryByText(/в этом входном диапазоне истории нет/)).not.toBeInTheDocument();
  expect(writes).toHaveLength(1);
});
it("reports known empty history separately and bounds the gap preview", () => {
  render(<ReplayPreflight preview={{ ...preview, results: [{ ...identity, cacheHit: false, status: "ready", coverage: { instrumentId: 1, fromUnix: 0, toUnix: 6000, splitUnix: 3000, expectedMinutes: 100, presentMinutes: 0, missingMinutes: 100, warmupExpectedMinutes: 50, warmupPresentMinutes: 0, signalExpectedMinutes: 50, signalPresentMinutes: 0, gaps: Array.from({ length: 51 }, (_, index) => ({ fromUnix: index * 60, toUnix: (index + 1) * 60 })), gapCount: 200, offset: 0, limit: 50, checkedAt: "2026-11-01T00:00:00Z" } }] }} />);
  expect(screen.getByText(/Проверено: в этом входном диапазоне истории нет/)).toBeVisible();
  expect(screen.getByText("Пропуски: показано 50 из 200 интервалов")).toBeVisible();
  expect(screen.getAllByRole("listitem", { hidden: true })).toHaveLength(50);
  expect(screen.queryByText(/Запуск недоступен:/)).not.toBeInTheDocument();
});
