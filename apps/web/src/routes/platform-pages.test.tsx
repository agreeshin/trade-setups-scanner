import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { AlgorithmsPage } from "./algorithms-page";
import { RunsPage } from "./runs-page";
import { SettingsPage } from "./settings-page";
import { SetupsPage } from "./setups-page";
import { previewSymbols } from "./instrument-picker";
import { HistoryPage } from "./history-page";
import { LiveInstruments } from "./live-instruments";

const instruments = [
  { id: 1, exchange: "bybit", category: "linear", symbol: "BTCUSDT", status: "Trading" },
  { id: 2, exchange: "bybit", category: "linear", symbol: "ETHUSDT", status: "Delisted" },
];

const algorithm = {
  id: "price_volume_spike", name: "Рост цены и объёма", version: "1", description: "Общие настройки", enabled: true,
  params: { priceGrowthPct: "12", windowMinutes: 5, volumeMultiplier: "3" },
  fields: [
    { key: "priceGrowthPct", label: "Рост цены", type: "decimal", min: 0, max: 100, unit: "%" },
    { key: "windowMinutes", label: "Окно", type: "integer", min: 1, max: 60, unit: "мин" },
    { key: "volumeMultiplier", label: "Множитель объёма", type: "decimal", min: 1, max: 100, unit: "×" },
  ],
};
const dropAlgorithm = {
  id: "price_drop_volume_spike", name: "Падение цены и всплеск оборота", version: "1.1.1", description: "Нейтральный WATCH: падение цены и оборот выше короткой или длинной базы. Общая конфигурация для истории и live.", enabled: true,
  params: { priceDropPct: "12", windowMinutes: 5, volumeMultiplier: "3" },
  fields: [
    { key: "priceDropPct", label: "Падение цены", type: "decimal", min: 0, max: 100, unit: "%" },
    { key: "windowMinutes", label: "Окно падения цены и оборота", type: "integer", min: 1, max: 60, unit: "мин" },
    { key: "volumeMultiplier", label: "Множитель объёма", type: "decimal", min: 1, max: 100, unit: "×" },
  ],
};
const runFixture = { id: 7, name: "Рост цены и объёма", symbol: "BTCUSDT", status: "done", cacheHit: false, setupCount: 3, error: null, algorithmId: "price_volume_spike", algorithmVersion: "1.1.1", fromUnix: 100, toUnix: 200, createdAt: "2026-01-01T00:00:00.000Z", params: {} };
let purgeResponses: { status: number; body: unknown }[] = [];
let replayRunsResponse: { runs: Record<string, unknown>[] } = { runs: [] };
const requests: { path: string; method: string; body: unknown }[] = [];
beforeEach(() => {
  requests.length = 0;
  replayRunsResponse = { runs: [] };
  purgeResponses = [];
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    requests.push({ path, method, body: typeof init?.body === "string" ? JSON.parse(init.body) as unknown : null });
    let data: unknown = {};
    if (path === "/v1/algorithms") data = { algorithms: [algorithm, dropAlgorithm] };
    if (path === "/v1/instruments") data = { instruments };
    if (path === "/v1/instrument-lists/live") data = { configured: false, instruments: [] };
    if (path === "/v1/history/backfill-jobs") data = { jobs: [{ id: 9, instrumentId: 1, fromUnix: 100, toUnix: 200, checkpointUnix: 150, status: "running", error: null }], counts: { queued: 0, running: 1, done: 0, cancel_requested: 0, cancelled: 0, failed: 0 } };
    if (path === "/v1/history/backfill-jobs/batch") data = { jobs: [], created: 1, declined: 0 };
    if (path === "/v1/history/backfill-plans" && method === "POST") data = { plan: { id: 3, fromUnix: 100, toUnix: 200, status: "active", counts: { total: 1, pending: 0, enqueued: 1, failed: 0, cancelled: 0 }, activeJobs: 1, createdAt: "2026-01-01T00:00:00Z", updatedAt: "2026-01-01T00:00:00Z" } };
    if (path === "/v1/history/backfill-plans" && method === "GET") data = { plans: [] };
    if (path === "/v1/history/collection-targets") data = { targets: [] };
    if (path === "/v1/history/gap-repairs") data = { jobs: [] };
    if (path === "/v1/history/instruments/1/coverage") data = { instrumentId: 1, firstMinuteUnix: null, lastMinuteUnix: null, totalCandles: 0, intervals: [], gaps: [] };
    if (path === "/v1/history/instruments/coverage-summary") data = { instruments: instruments.map((instrument) => ({ instrumentId: instrument.id, exchange: instrument.exchange, category: instrument.category, symbol: instrument.symbol, status: instrument.status, firstMinuteUnix: null, lastMinuteUnix: null, totalCandles: 0, missingMinutes: 0, verifiedMinutes: 0 })) };
    if (path === "/v1/settings/ui") data = { value: { existing: "keep" } };
    if (path === "/v1/replay-runs") data = replayRunsResponse;
    if (method === "PATCH" && /^\/v1\/replay-(runs|batches)\/\d+$/.test(path)) {
      const { name } = JSON.parse(typeof init?.body === "string" ? init.body : "{}") as { name: string };
      replayRunsResponse = { runs: replayRunsResponse.runs.map((run) => path === `/v1/replay-${run.isBatch === true ? "batches" : "runs"}/${String(run.id)}` ? { ...run, name } : run) };
      return Promise.resolve(new Response(JSON.stringify({ ok: true, name })));
    }
    if (path.startsWith("/v1/replay-batches/")) {
      if (path.endsWith("/cancel")) data = { cancelled: 1, cancelRequested: 0 };
      else if (path.endsWith("/export.csv")) return Promise.resolve(new Response("signal,exchange,category,symbol\r\n", { status: 200, headers: { "content-type": "text/csv; charset=utf-8" } }));
      else data = { run: { id: 7, name: "Пакетный прогон", isBatch: true, exchange: "bybit", category: "linear", symbol: "2 инструментов", status: "running", cacheHit: false, setupCount: 1, warmupMinutes: null, processedMinutes: null, error: null, algorithmId: "price_volume_spike", algorithmVersion: "1.1.1", fromUnix: 100, toUnix: 200, createdAt: "2026-01-01T00:00:00.000Z", params: {}, instrumentCount: 2, activeInstruments: 1, doneInstruments: 1, failedInstruments: 0, cancelledInstruments: 0 }, setups: [{ exchange: "bybit", category: "linear", symbol: "AAAUSDT", algorithmId: "price_volume_spike", openTimeUnix: 1, closeTimeUnix: 2, closePrice: "3" }], offset: 0, limit: 100, total: 1 };
    }
    if (path === "/v1/replay-preflight") data = { checkedAt: "2026-10-10T10:00:00Z", algorithmVersion: "snapshot-v2", requiredHistoryMinutes: 43200, canRun: true, request: { ...(JSON.parse(typeof init?.body === "string" ? init.body : "{}") as Record<string, unknown>), async: true, algorithmVersion: "snapshot-v2", params: { priceGrowthPct: "17", windowMinutes: 9 } }, results: [] };
    if (path === "/v1/settings/notifications") data = { maxAgeSeconds: 60 };
    if (path === "/v1/live/status") data = { enabled: false, running: false, lastScanAt: null, lastError: null };
    if (path === "/v1/replay-cache/stats") data = { entries: 3, bytes: 1024 };
    if (path === "/v1/replay-cache") data = { removed: 3 };
    if (path.startsWith("/v1/setups?")) {
      const params = new URL(path, "http://localhost").searchParams;
      const offset = Number(params.get("offset"));
      const limit = Number(params.get("limit"));
      const all = Array.from({ length: 601 }, (_, index) => ({ symbol: `BTC${index}`, exchange: "bybit", algorithmId: "price_volume_spike", openTimeUnix: 100, closeTimeUnix: 200 + index, closePrice: "100" }));
      const filtered = all.filter((row) => row.symbol.includes(params.get("q") ?? ""));
      data = { setups: filtered.slice(offset, offset + limit), offset, limit, total: filtered.length, snapshotId: 601 };
    }
    if (path === "/v1/replay-runs/purge") {
      const next = purgeResponses.shift() ?? { status: 200, body: { runsDeleted: 0, cacheDeleted: 0, qualityIntervalsDeleted: 0 } };
      return Promise.resolve(new Response(JSON.stringify(next.body), { status: next.status }));
    }
    return Promise.resolve(new Response(JSON.stringify(data), { status: 200 }));
  }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

describe("platform screens", () => {
  it.each([
    { isBatch: false, path: "/v1/replay-runs/7", trigger: "enter" },
    { isBatch: true, path: "/v1/replay-batches/7", trigger: "blur" },
  ])("renames a run through $path on $trigger and reloads the list", async ({ isBatch, path, trigger }) => {
    replayRunsResponse = { runs: [{ ...runFixture, isBatch }] };
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole("button", { name: "Переименовать запуск 7" }));
    const input = screen.getByRole("textbox", { name: "Имя запуска 7" });
    expect(input).toHaveFocus();
    expect(input).toHaveValue(runFixture.name);
    fireEvent.change(input, { target: { value: "  Утренний прогон  " } });
    if (trigger === "enter") await userEvent.keyboard("{Enter}");
    else await userEvent.tab();
    expect(await screen.findByText("Утренний прогон")).toHaveAttribute("title", "Утренний прогон");
    expect(requests.filter((request) => request.method === "PATCH")).toEqual([{ path, method: "PATCH", body: { name: "Утренний прогон" } }]);
    expect(requests.filter((request) => request.path === "/v1/replay-runs")).toHaveLength(2);
    expect(screen.queryByRole("textbox", { name: "Имя запуска 7" })).not.toBeInTheDocument();
  });

  it("cancels run name editing with Escape and skips unchanged names", async () => {
    replayRunsResponse = { runs: [runFixture] };
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole("button", { name: "Переименовать запуск 7" }));
    fireEvent.change(screen.getByRole("textbox", { name: "Имя запуска 7" }), { target: { value: "Не сохранять" } });
    await userEvent.keyboard("{Escape}");
    expect(screen.queryByRole("textbox", { name: "Имя запуска 7" })).not.toBeInTheDocument();
    expect(screen.getByTitle(runFixture.name)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Переименовать запуск 7" }));
    expect(screen.getByRole("textbox", { name: "Имя запуска 7" })).toHaveValue(runFixture.name);
    await userEvent.keyboard("{Enter}");
    expect(screen.queryByRole("textbox", { name: "Имя запуска 7" })).not.toBeInTheDocument();
    expect(requests.some((request) => request.method === "PATCH")).toBe(false);
  });

  it.each([
    { value: "", message: "Введите имя запуска." },
    { value: "   ", message: "Введите имя запуска." },
    { value: "Я".repeat(121), message: "Имя запуска должно содержать не более 120 символов." },
  ])("rejects an invalid run name before PATCH: $value", async ({ value, message }) => {
    replayRunsResponse = { runs: [runFixture] };
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole("button", { name: "Переименовать запуск 7" }));
    const input = screen.getByRole("textbox", { name: "Имя запуска 7" });
    fireEvent.change(input, { target: { value } });
    await userEvent.keyboard("{Enter}");
    expect(screen.getByRole("alert")).toHaveTextContent(message);
    expect(input).toHaveAttribute("aria-invalid", "true");
    expect(input).toHaveAccessibleDescription(message);
    fireEvent.blur(input);
    expect(requests.some((request) => request.method === "PATCH")).toBe(false);
  });

  it("accepts a trimmed name of exactly 120 characters", async () => {
    replayRunsResponse = { runs: [runFixture] };
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole("button", { name: "Переименовать запуск 7" }));
    const name = "Я".repeat(120);
    fireEvent.change(screen.getByRole("textbox", { name: "Имя запуска 7" }), { target: { value: `  ${name}  ` } });
    await userEvent.keyboard("{Enter}");
    expect(await screen.findByTitle(name)).toHaveClass("truncate");
    expect(requests.find((request) => request.method === "PATCH")?.body).toEqual({ name });
  });

  it.each([400, 404, 503, 0])("preserves the draft after a rename failure (%i) and allows retry", async (status) => {
    replayRunsResponse = { runs: [runFixture] };
    const original = globalThis.fetch;
    let failing = true;
    vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
      if (init?.method === "PATCH" && failing) return status === 0 ? Promise.reject(new Error("network offline")) : Promise.resolve(new Response("{}", { status }));
      return original(path, init);
    }));
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole("button", { name: "Переименовать запуск 7" }));
    const input = screen.getByRole("textbox", { name: "Имя запуска 7" });
    fireEvent.change(input, { target: { value: "Мой прогон" } });
    await userEvent.keyboard("{Enter}");
    expect(await screen.findByRole("alert")).toHaveTextContent(status === 0 ? "network offline" : `Не удалось сохранить имя запуска: HTTP ${status}`);
    expect(input).toHaveValue("Мой прогон");
    expect(input).toBeEnabled();
    failing = false;
    await userEvent.keyboard("{Enter}");
    expect(await screen.findByTitle("Мой прогон")).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("disables an in-flight rename, deduplicates Enter and blur, and aborts on unmount", async () => {
    replayRunsResponse = { runs: [runFixture] };
    const original = globalThis.fetch;
    const pending: { resolve?: (response: Response) => void; signal?: AbortSignal | null } = {};
    const patch = vi.fn((init?: RequestInit) => {
      pending.signal = init?.signal ?? null;
      return new Promise<Response>((resolve) => { pending.resolve = resolve; });
    });
    vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => init?.method === "PATCH" ? patch(init) : original(path, init)));
    const view = render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole("button", { name: "Переименовать запуск 7" }));
    const input = screen.getByRole("textbox", { name: "Имя запуска 7" });
    fireEvent.change(input, { target: { value: "Мой прогон" } });
    fireEvent.keyDown(input, { key: "Enter" });
    fireEvent.blur(input);
    expect(input).toBeDisabled();
    expect(screen.getByText("Сохранение…")).toHaveAttribute("role", "status");
    expect(patch).toHaveBeenCalledOnce();
    view.unmount();
    expect(pending.signal?.aborted).toBe(true);
    await act(async () => { pending.resolve?.(new Response(JSON.stringify({ ok: true, name: "Мой прогон" }))); await Promise.resolve(); });
    expect(requests.filter((request) => request.path === "/v1/replay-runs")).toHaveLength(1);
  });

  it("preserves the run name draft across the three-second polling refresh", async () => {
    vi.useFakeTimers();
    replayRunsResponse = { runs: [runFixture] };
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    fireEvent.click(screen.getByRole("button", { name: "Переименовать запуск 7" }));
    const input = screen.getByRole("textbox", { name: "Имя запуска 7" });
    fireEvent.change(input, { target: { value: "Мой черновик" } });
    replayRunsResponse = { runs: [{ ...runFixture, name: "Обновлённое имя с сервера", setupCount: 17 }] };
    await act(async () => { await vi.advanceTimersByTimeAsync(3000); });
    expect(requests.filter((request) => request.path === "/v1/replay-runs")).toHaveLength(2);
    expect(screen.getByText("17")).toBeInTheDocument();
    expect(screen.getByRole("textbox", { name: "Имя запуска 7" })).toBe(input);
    expect(input).toHaveValue("Мой черновик");
    expect(input).toHaveFocus();
  });

  it("centers setup counts and run actions and leaves the launch card full width", async () => {
    replayRunsResponse = { runs: [runFixture, { ...runFixture, id: 8, status: "running" }] };
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    const table = await screen.findByRole("table");
    expect(within(table).getAllByRole("columnheader").map((head) => head.textContent)).toEqual(["ID", "Имя", "Инструмент", "Алгоритм", "Статус", "Кеш", "Сетапов", "Действие"]);
    expect(within(table).getByRole("columnheader", { name: "Сетапов" })).toHaveClass("text-center");
    expect(within(table).getByRole("columnheader", { name: "Действие" })).toHaveClass("text-center");
    for (const row of within(table).getAllByRole("row").slice(1)) {
      const cells = within(row).getAllByRole("cell");
      expect(cells[6]).toHaveClass("text-center", "tabular-nums");
      expect(cells[6]).not.toHaveClass("text-right");
      expect(cells[7]?.firstElementChild).toHaveClass("flex", "items-center", "justify-center");
    }
    expect(screen.getByLabelText("Начало").closest("form")?.className).not.toMatch(/max-w-/);
  });

  it("keeps settings sections full width on one surface with a shared sidebar", async () => {
    const { container } = render(<SettingsPage />);
    await screen.findByRole("button", { name: "Включить live" });
    const wrappers = container.querySelectorAll("section[aria-labelledby], form[aria-labelledby], [data-slot='card']");
    expect(wrappers.length).toBeGreaterThan(5);
    expect(container.querySelector('[data-slot="page-sidebar"]')).toBeInTheDocument();
    expect(container.querySelector('[data-slot="card"]')).toBeNull();
    expect(container.querySelector('[data-slot="segmented-list"]')).toBeNull();
    for (const wrapper of wrappers) expect(wrapper.className).not.toMatch(/max-w-/);
  });

  it("purges all replay runs after two-step confirmation", async () => {
    purgeResponses = [{ status: 200, body: { runsDeleted: 2, cacheDeleted: 1, qualityIntervalsDeleted: 0 } }];
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole("button", { name: "Очистить все прогоны" }));
    expect(screen.getByRole("button", { name: "Подтвердить очистку всего" })).toBeInTheDocument();
    expect(requests.some((request) => request.path === "/v1/replay-runs/purge")).toBe(false);
    await userEvent.click(screen.getByRole("button", { name: "Подтвердить очистку всего" }));
    await waitFor(() => expect(screen.getByText(/Удалено: прогонов 2/)).toBeInTheDocument());
    expect(requests.some((request) => request.path === "/v1/replay-runs/purge" && request.method === "POST")).toBe(true);
  });

  it("completes the purge automatically after stopping active runs", async () => {
    purgeResponses = [
      { status: 409, body: { error: "active_runs", active: 1, queuedCancelled: 5, runningCancelRequested: 1 } },
      { status: 200, body: { runsDeleted: 6, cacheDeleted: 4, qualityIntervalsDeleted: 2 } },
    ];
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole("button", { name: "Очистить все прогоны" }));
    await userEvent.click(screen.getByRole("button", { name: "Подтвердить очистку всего" }));
    await waitFor(() => expect(screen.getByText(/Удалено: прогонов 6/)).toBeInTheDocument(), { timeout: 15000 });
  }, 20000);

  it("submits algorithm and local dates without overriding saved algorithm params", async () => {
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await waitFor(() => expect(screen.getByRole("button", { name: "Проверить данные" })).toBeEnabled());
    expect(screen.getByRole("checkbox", { name: "BTCUSDT (Trading)" })).toBeChecked();
    expect(screen.getByRole("checkbox", { name: "ETHUSDT (Delisted)" })).toBeChecked();
    fireEvent.change(screen.getByLabelText("Начало"), { target: { value: "2026-10-10T12:00" } });
    fireEvent.change(screen.getByLabelText("Конец"), { target: { value: "2026-10-11T13:00" } });
    await userEvent.click(screen.getByRole("button", { name: "Проверить данные" }));
    expect(requests.some((request) => request.path === "/v1/replay-runs" && request.method === "POST")).toBe(false);
    await userEvent.click(await screen.findByRole("button", { name: "Запустить проверенный расчёт" }));
    await screen.findByText(/Запрос запуска принят/);
    expect(requests.find((request) => request.path === "/v1/replay-runs" && request.method === "POST")?.body).toEqual({ async: true, algorithmVersion: "snapshot-v2", params: { priceGrowthPct: "17", windowMinutes: 9 }, algorithmId: "price_volume_spike", instruments: [{ exchange: "bybit", category: "linear", symbol: "BTCUSDT" }, { exchange: "bybit", category: "linear", symbol: "ETHUSDT" }], fromUnix: Date.UTC(2026, 9, 10) / 1000, toUnix: Date.UTC(2026, 9, 11) / 1000 });
    expect(requests.some((request) => request.method === "PUT")).toBe(false);
    await userEvent.click(screen.getByRole("button", { name: "Сохранить форму" }));
    await screen.findByText("Форма сохранена в базе сигналов.");
    expect(requests.find((request) => request.path === "/v1/settings/ui" && request.method === "PUT")?.body).toMatchObject({ value: { existing: "keep", replay: { algorithmId: "price_volume_spike" } } });
  });
  it("blocks a reversed date range before POST", async () => {
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await waitFor(() => expect(screen.getByRole("button", { name: "Проверить данные" })).toBeEnabled());
    fireEvent.change(screen.getByLabelText("Начало"), { target: { value: "2026-10-10T13:00" } });
    fireEvent.change(screen.getByLabelText("Конец"), { target: { value: "2026-10-10T12:00" } });
    await userEvent.click(screen.getByRole("button", { name: "Проверить данные" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("конец диапазона должен быть позже начала");
    expect(requests.some((request) => request.method === "POST")).toBe(false);
  });
  it("keeps percent and minute units distinct when saving an algorithm", async () => {
    render(<AlgorithmsPage />);
    expect(screen.getByRole("heading", { level: 1, name: "Алгоритмы" })).toHaveClass("sr-only");
    const growth = await screen.findByLabelText("Рост цены (%)");
    expect(growth).toHaveValue(12);
    expect(screen.getByLabelText("Окно (мин)")).toHaveValue(5);
    fireEvent.change(growth, { target: { value: "15" } });
    await userEvent.click(screen.getAllByRole("button", { name: "Сохранить алгоритм" })[0]!);
    await screen.findByText("Настройки алгоритма сохранены.");
    expect(requests.find((request) => request.path === "/v1/algorithms/price_volume_spike")).toEqual({ path: "/v1/algorithms/price_volume_spike", method: "PUT", body: { params: { priceGrowthPct: "15", windowMinutes: 5, volumeMultiplier: "3" }, enabled: true } });
  });

  it("defaults settings to scanning without algorithm controls and requires cache confirmation", async () => {
    render(<SettingsPage />);
    expect(screen.getAllByRole("tab").map((tab) => tab.textContent)).toEqual(["Сканирование", "Уведомления", "Обслуживание", "Интерфейс"]);
    expect(screen.getByRole("tab", { name: "Сканирование" })).toHaveAttribute("aria-selected", "true");
    expect(screen.getByRole("heading", { name: "Live-сканирование" })).toBeVisible();
    expect(screen.queryByRole("tab", { name: "Алгоритмы" })).not.toBeInTheDocument();
    expect(screen.queryByLabelText("Рост цены (%)")).not.toBeInTheDocument();
    expect(requests.some((request) => request.path === "/v1/algorithms")).toBe(false);
    await userEvent.click(screen.getByRole("tab", { name: "Уведомления" }));
    const age = await screen.findByRole("spinbutton", { name: "Максимальный возраст уведомления (секунд)" });
    fireEvent.change(age, { target: { value: "99" } });
    expect(screen.getByText(/Telegram не подключён/)).toBeVisible();
    expect(screen.getByText(/Web Push недоступен по обычному HTTP/)).toHaveTextContent("localhost");
    await userEvent.click(screen.getByRole("tab", { name: "Обслуживание" }));
    expect(screen.getByRole("button", { name: "Очистить кеш" })).toBeDisabled();
    await userEvent.click(screen.getByLabelText("Подтверждаю очистку всего кеша"));
    await userEvent.click(screen.getByRole("button", { name: "Очистить кеш" }));
    await screen.findByText("Удалено записей кеша: 3.");
    expect(screen.getByRole("button", { name: "Очистить кеш" })).toBeDisabled();
    expect(requests.some((request) => request.path === "/v1/settings/live" && request.method === "PUT")).toBe(false);
    await userEvent.click(screen.getByRole("tab", { name: "Уведомления" }));
    expect(screen.getByRole("spinbutton", { name: "Максимальный возраст уведомления (секунд)" })).toBe(age);
    expect(age).toHaveValue(99);
    expect(requests.filter((request) => request.path === "/v1/settings/notifications")).toHaveLength(1);
  });

  it("retains both algorithm forms but shows and saves only the selected one", async () => {
    const { container } = render(<AlgorithmsPage />);
    expect(await screen.findByLabelText("Рост цены (%)")).toHaveValue(12);
    expect(screen.getByText("Рост цены и объёма · 1")).toBeInTheDocument();
    expect(screen.getByText("Падение цены и всплеск оборота · 1.1.1")).toBeInTheDocument();
    expect(screen.getByText("Падение цены и всплеск оборота · 1.1.1")).not.toBeVisible();
    await userEvent.click(screen.getByRole("tab", { name: "Падение цены и всплеск оборота" }));
    expect(screen.getAllByRole("tabpanel")).toHaveLength(1);
    const drop = screen.getByLabelText("Падение цены (%)");
    expect(drop).toHaveValue(12);
    expect(screen.getByLabelText("Окно падения цены и оборота (мин)")).toHaveValue(5);
    fireEvent.change(drop, { target: { value: "9" } });
    await userEvent.click(screen.getByRole("button", { name: "Сохранить алгоритм" }));
    await screen.findByText("Настройки алгоритма сохранены.");
    expect(requests.find((request) => request.path === "/v1/algorithms/price_drop_volume_spike")).toEqual({ path: "/v1/algorithms/price_drop_volume_spike", method: "PUT", body: { params: { priceDropPct: "9", windowMinutes: 5, volumeMultiplier: "3" }, enabled: true } });
    for (const wrapper of container.querySelectorAll("section[aria-labelledby], [data-slot='card']")) expect(wrapper.className).not.toMatch(/max-w-/);
  });

  it("saves interface date and time settings through the ui settings document", async () => {
    render(<SettingsPage />);
    await userEvent.click(screen.getByRole("tab", { name: "Интерфейс" }));
    fireEvent.change(await screen.findByLabelText("Порядок даты"), { target: { value: "ymd" } });
    fireEvent.change(screen.getByLabelText("Разделитель даты"), { target: { value: "-" } });
    fireEvent.change(screen.getByLabelText("Формат времени"), { target: { value: "12" } });
    await userEvent.click(screen.getByRole("button", { name: "Сохранить и применить" }));
    await waitFor(() => expect(screen.getByText(/Настройки интерфейса сохранены/)).toBeInTheDocument());
    const put = requests.find((request) => request.path === "/v1/settings/ui" && request.method === "PUT");
    expect(put?.body).toEqual({ value: { existing: "keep", interface: { dateOrder: "ymd", dateSeparator: "-", yearDigits: 4, time24: false, timezone: "auto" } } });
  });

  it("deletes single and batch finished runs from the recent runs table", async () => {
    replayRunsResponse = { runs: [
      { id: 7, name: "Рост цены и объёма", symbol: "BTCUSDT", status: "done", cacheHit: false, setupCount: 3, error: null, algorithmId: "price_volume_spike", algorithmVersion: "1.1.1", fromUnix: 100, toUnix: 200, createdAt: "2026-01-01T00:00:00.000Z" },
      { id: 8, name: "Пакетный прогон", isBatch: true, symbol: "2 инструментов", status: "done", cacheHit: false, setupCount: 5, error: null, algorithmId: "price_volume_spike", algorithmVersion: "1.1.1", fromUnix: 100, toUnix: 200, createdAt: "2026-01-02T00:00:00.000Z", instrumentCount: 2, activeInstruments: 0, doneInstruments: 2, failedInstruments: 0, cancelledInstruments: 0 },
    ] };
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole("button", { name: "Удалить запуск 7" }));
    expect(await screen.findByText("Удалить запуск #7?")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Удалить" }));
    await waitFor(() => expect(screen.getByText("Запуск #7 удалён.")).toBeInTheDocument());
    expect(requests.some((request) => request.path === "/v1/replay-runs/7" && request.method === "DELETE")).toBe(true);
    await userEvent.click(screen.getByRole("button", { name: "Удалить запуск 8" }));
    expect(await screen.findByText("Удалить запуск #8?")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Удалить" }));
    await waitFor(() => expect(screen.getByText("Запуск #8 удалён.")).toBeInTheDocument());
    expect(requests.some((request) => request.path === "/v1/replay-batches/8" && request.method === "DELETE")).toBe(true);
  });
  it("requests server pages and finds records beyond the former latest-500 cap", async () => {
    render(<SetupsPage />);
    await screen.findByText("Страница 1 из 25 · Найдено: 601");
    expect(screen.queryByText("bybit · BTC29")).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Далее" }));
    await screen.findByText("bybit · BTC29");
    expect(requests.some((request) => request.path.includes("offset=25&limit=25&q=&snapshotId=601"))).toBe(true);
    fireEvent.change(screen.getByLabelText("Инструмент, биржа или алгоритм"), { target: { value: "BTC600" } });
    await screen.findByText("Страница 1 из 1 · Найдено: 1");
    expect(screen.getByText("bybit · BTC600")).toBeInTheDocument();
    expect(requests.some((request) => request.path === "/v1/setups?offset=0&limit=25&q=BTC600")).toBe(true);
    expect(screen.getByRole("link", { name: "Экспорт CSV (без фильтра)" })).toHaveAttribute("href", "/v1/export/setups.csv");
    await userEvent.click(screen.getByRole("button", { name: "Обновить" }));
    await screen.findByText("bybit · BTC600");
    expect(requests.some((request) => request.method === "PUT")).toBe(false);
  });
  it("aborts stale feed requests and ignores their late responses", async () => {
    let resolveSlow: ((response: Response) => void) | undefined;
    const signals: (AbortSignal | null | undefined)[] = [];
    vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
      if (path === "/v1/settings/ui") return Promise.resolve(new Response(JSON.stringify({ value: {} })));
      signals.push(init?.signal);
      if (path.includes("q=slow")) return new Promise<Response>((resolve) => { resolveSlow = resolve; });
      const symbol = path.includes("q=fast") ? "FAST" : "INITIAL";
      return Promise.resolve(new Response(JSON.stringify({ setups: [{ symbol, exchange: "bybit", algorithmId: "price_volume_spike", openTimeUnix: 100, closeTimeUnix: 200, closePrice: "100" }], total: 1, limit: 25, offset: 0, snapshotId: 1 })));
    }));
    const { unmount } = render(<SetupsPage />);
    await screen.findByText("bybit · INITIAL");
    const filter = screen.getByLabelText("Инструмент, биржа или алгоритм");
    fireEvent.change(filter, { target: { value: "slow" } });
    await waitFor(() => { expect(resolveSlow).toBeDefined(); });
    const slowSignal = signals.at(-1);
    fireEvent.change(filter, { target: { value: "fast" } });
    await screen.findByText("bybit · FAST");
    expect(slowSignal?.aborted).toBe(true);
    await act(async () => {
      resolveSlow?.(new Response(JSON.stringify({ setups: [], total: 0, limit: 25, offset: 0, snapshotId: 0 })));
      await Promise.resolve();
    });
    expect(screen.getByText("bybit · FAST")).toBeInTheDocument();
    const lastSignal = signals.at(-1);
    unmount();
    expect(lastSignal?.aborted).toBe(true);
  });
  it("reports unavailable algorithms instead of enabling a fabricated run", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("{}", { status: 503 }))));
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await screen.findByText("Алгоритмы недоступны (HTTP 503)");
    expect(screen.getByRole("button", { name: "Проверить данные" })).toBeDisabled();
  });
  it("previews CSV exclusions and duplicates before replacing the catalog selection", async () => {
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await screen.findByRole("checkbox", { name: "BTCUSDT (Trading)" });
    fireEvent.change(screen.getByLabelText("CSV: один символ на строку"), { target: { value: "symbol\nbtcusdt\nBTCUSDT\nUNKNOWN\nBTCUSDT,ETHUSDT" } });
    await userEvent.click(screen.getByRole("button", { name: "Проверить импорт" }));
    expect(screen.getByRole("status")).toHaveTextContent("Принято: 1. Дубли: 1. Исключено: 3.");
    expect(screen.getByRole("checkbox", { name: "ETHUSDT (Delisted)" })).toBeChecked();
    await userEvent.click(screen.getByRole("button", { name: "Заменить выбор принятым списком" }));
    expect(screen.getByRole("checkbox", { name: "BTCUSDT (Trading)" })).toBeChecked();
    expect(screen.getByRole("checkbox", { name: "ETHUSDT (Delisted)" })).not.toBeChecked();
    await userEvent.click(screen.getByRole("checkbox", { name: "ETHUSDT (Delisted)" }));
    expect(screen.getByRole("checkbox", { name: "ETHUSDT (Delisted)" })).toBeChecked();
  });
  it("parses headerless single-column CSV with BOM and quoted symbols", () => {
    expect(previewSymbols('\uFEFF"BTCUSDT"\r\nETHUSDT\r\nBTCUSDT\r\n', instruments)).toEqual({ accepted: ["BTCUSDT", "ETHUSDT"], excluded: [], duplicates: 1 });
  });
  it("shows saved run params, WATCH and external chart links", async () => {
    const original = globalThis.fetch;
    const run = { id: 7, name: "Прогон с сохранёнными параметрами", symbol: "BTCUSDT", status: "completed", cacheHit: false, setupCount: 1, error: null, algorithmId: "price_volume_spike", algorithmVersion: "1", fromUnix: 100, toUnix: 300, params: { priceGrowthPct: "17", windowMinutes: 5 } };
    vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
      if (path === "/v1/replay-runs") return Promise.resolve(new Response(JSON.stringify({ runs: [run] })));
      if (path === "/v1/replay-runs/7?offset=0&limit=100") return Promise.resolve(new Response(JSON.stringify({ run, offset: 0, limit: 100, total: 1, setups: [{ exchange: "bybit", category: "linear", symbol: "BTCUSDT", algorithmId: "price_volume_spike", openTimeUnix: 100, closeTimeUnix: 200, closePrice: "100" }] })));
      return original(path, init);
    }));
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole("button", { name: "Результаты запуска 7" }));
    await userEvent.click(await screen.findByText("Снимок параметров запуска"));
    expect(screen.getByText("Прогон с сохранёнными параметрами · BTCUSDT · price_volume_spike · 1 · completed")).toBeVisible();
    expect(screen.getByText("17")).toBeVisible();
    expect(screen.getByText("WATCH")).toBeVisible();
    expect(screen.getByRole("link", { name: "TradingView" })).toHaveAttribute("href", "https://www.tradingview.com/chart/?symbol=BYBIT%3ABTCUSDT.P");
    expect(screen.getByRole("link", { name: "Bybit" })).toHaveAttribute("rel", "noopener noreferrer");
  });
  it("loads history without enqueuing and only mutates on explicit actions", async () => {
    render(<HistoryPage />);
    await userEvent.click(screen.getByRole("tab", { name: "Задания загрузки" }));
    await screen.findByRole("button", { name: "Отменить загрузку 9" });
    expect(requests.every((request) => request.method === "GET")).toBe(true);
    await userEvent.click(screen.getByRole("tab", { name: "Текущий сбор свечей" }));
    await userEvent.selectOptions(screen.getByLabelText("Инструмент БД"), "1");
    await screen.findByText("В БД нет свечей этого инструмента.");
    await userEvent.click(screen.getByRole("tab", { name: "Загрузка истории" }));
    await userEvent.click(await screen.findByRole("checkbox", { name: "BTCUSDT (Trading)" }));
    fireEvent.change(screen.getByLabelText("Начало загрузки"), { target: { value: "2026-10-10T12:00" } });
    fireEvent.change(screen.getByLabelText("Конец загрузки"), { target: { value: "2026-10-11T13:00" } });
    expect(screen.getByLabelText("Начало загрузки").closest("form")?.className).not.toMatch(/max-w-/);
    await userEvent.click(screen.getByRole("button", { name: "Создать план загрузки (1)" }));
    await screen.findByText(/Создан план загрузки #3: 1 инструментов/);
    expect(requests.find((request) => request.path === "/v1/history/backfill-plans" && request.method === "POST")?.body).toMatchObject({ instrumentIds: [1], fromUnix: Date.UTC(2026, 9, 10) / 1000 });
    await userEvent.click(screen.getByRole("tab", { name: "Задания загрузки" }));
    await userEvent.click(screen.getByRole("button", { name: "Отменить загрузку 9" }));
    await screen.findByText("Отмена задания #9 запрошена.");
    expect(requests.some((request) => request.path === "/v1/history/backfill-jobs/9/cancel" && request.method === "POST")).toBe(true);
    await userEvent.click(screen.getByRole("tab", { name: "Текущий сбор свечей" }));
    expect(screen.getByRole("button", { name: "Импортировать листинг для сбора" }).closest("[data-slot='card']")).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "Включить сбор" }));
    await screen.findByText("Настройка сбора сохранена.");
    expect(requests.find((request) => request.path === "/v1/history/collection-targets/1")?.body).toEqual({ enabled: true });
  });
  it("loads heavy history panels lazily and retains their filters after switching the sidebar", async () => {
    const { container } = render(<HistoryPage />);
    await screen.findByRole("checkbox", { name: "BTCUSDT (Trading)" });
    expect(requests.some((request) => request.path.endsWith("/coverage-summary"))).toBe(false);
    expect(container.querySelector('[data-slot="page-sidebar"]')).toBeInTheDocument();
    expect(container.querySelector('[data-slot="segmented-list"]')).toBeNull();
    await userEvent.click(screen.getByRole("tab", { name: "Текущий сбор свечей" }));
    const filter = await screen.findByRole("textbox", { name: "Фильтр по символу" });
    fireEvent.change(filter, { target: { value: "ETH" } });
    await userEvent.click(screen.getByRole("checkbox", { name: "Только инструменты со свечами" }));
    await userEvent.click(screen.getByRole("tab", { name: "Загрузка истории" }));
    expect(filter).toBeInTheDocument(); expect(filter).not.toBeVisible();
    await userEvent.click(screen.getByRole("tab", { name: "Текущий сбор свечей" }));
    expect(screen.getByRole("textbox", { name: "Фильтр по символу" })).toBe(filter);
    expect(filter).toHaveValue("ETH");
    expect(screen.getByRole("checkbox", { name: "Только инструменты со свечами" })).not.toBeChecked();
    expect(screen.getAllByRole("tabpanel")).toHaveLength(1);
    expect(requests.filter((request) => request.path.endsWith("/coverage-summary"))).toHaveLength(1);
    expect(requests.every((request) => request.method === "GET")).toBe(true);
  });
  it("distinguishes unavailable coverage from empty history", async () => {
    const original = globalThis.fetch;
    vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => path.includes("/coverage") ? Promise.resolve(new Response("{}", { status: 503 })) : original(path, init)));
    render(<HistoryPage />);
    await userEvent.click(screen.getByRole("tab", { name: "Текущий сбор свечей" }));
    await waitFor(() => expect(screen.getByLabelText("Инструмент БД")).toBeEnabled());
    await userEvent.selectOptions(screen.getByLabelText("Инструмент БД"), "1");
    await screen.findByText(/Покрытие неизвестно: API недоступен/);
    expect(screen.queryByText("В БД нет свечей этого инструмента.")).not.toBeInTheDocument();
  });
  it("defaults live selection to active only and saves an explicit empty list without enabling scanner", async () => {
    const original = globalThis.fetch;
    vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => path === "/v1/instruments" ? Promise.resolve(new Response(JSON.stringify({ instruments: [{ ...instruments[0], status: "active" }, instruments[1]] }))) : original(path, init)));
    render(<LiveInstruments />);
    expect(await screen.findByRole("checkbox", { name: "BTCUSDT (active)" })).toBeChecked();
    expect(screen.queryByRole("checkbox", { name: "ETHUSDT (Delisted)" })).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Убрать все" }));
    expect(requests.every((request) => request.method === "GET")).toBe(true);
    await userEvent.click(screen.getByRole("button", { name: "Сохранить live-список" }));
    await screen.findByText("Live-список сохранён. Состояние сканера не изменено.");
    expect(requests.find((request) => request.path === "/v1/instrument-lists/live" && request.method === "PUT")?.body).toEqual({ instruments: [] });
    expect(requests.some((request) => request.path === "/v1/settings/live")).toBe(false);
  });
  it("requests replay cancellation without reporting premature completion", async () => {
    const original = globalThis.fetch;
    vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => path === "/v1/replay-runs" ? Promise.resolve(new Response(JSON.stringify({ runs: [{ id: 12, name: "Рост цены и объёма", symbol: "BTCUSDT", status: "queued", cacheHit: false, setupCount: 0, algorithmId: "price_volume_spike", algorithmVersion: "1" }] }))) : original(path, init)));
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole("button", { name: "Отменить запуск 12" }));
    await screen.findByText("Отмена запуска #12 запрошена. Дождитесь конечного статуса.");
    expect(requests.some((request) => request.path === "/v1/replay-runs/12/cancel" && request.method === "POST")).toBe(true);
  });
  it("pages run results server-side, preserves the page during refresh and downloads the complete CSV", async () => {
    const original = globalThis.fetch;
    const detailRequests: string[] = [];
    const run = { id: 7, name: "Рост цены и объёма", symbol: "BTCUSDT", status: "completed", cacheHit: false, setupCount: 101, algorithmId: "price_volume_spike", algorithmVersion: "1", fromUnix: 100, toUnix: 300, params: {} };
    const createObjectURL = vi.fn(() => "blob:test-run");
    const revokeObjectURL = vi.fn();
    vi.stubGlobal("URL", { createObjectURL, revokeObjectURL });
    const click = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => undefined);
    vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
      if (path === "/v1/replay-runs") return Promise.resolve(new Response(JSON.stringify({ runs: [run, { ...run, id: 8 }] })));
      if (path.includes("/export.csv")) { requests.push({ path, method: init?.method ?? "GET", body: null }); return Promise.resolve(new Response("symbol\nBTCUSDT\n", { headers: { "content-type": "text/csv" } })); }
      if (path.includes("?offset=")) {
        detailRequests.push(path);
        const offset = path.includes("offset=100") ? 100 : 0;
        return Promise.resolve(new Response(JSON.stringify({ run, offset, limit: 100, total: 101, setups: [{ exchange: "bybit", symbol: offset === 100 ? "PAGE_TWO" : "PAGE_ONE", algorithmId: "price_volume_spike", openTimeUnix: 100, closeTimeUnix: 200, closePrice: "1" }] })));
      }
      return original(path, init);
    }));
    const view = render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole("button", { name: "Результаты запуска 7" }));
    await screen.findByText("Всего сетапов: 101. Страница 1 из 2");
    await userEvent.click(screen.getByRole("button", { name: "Следующая страница" }));
    await screen.findByText("bybit · PAGE_TWO");
    expect(screen.queryByText("bybit · PAGE_ONE")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Следующая страница" })).toBeDisabled();
    await userEvent.click(screen.getByRole("button", { name: "Обновить" }));
    await waitFor(() => expect(detailRequests.filter((path) => path === "/v1/replay-runs/7?offset=100&limit=100")).toHaveLength(2));
    expect(screen.getByText("Всего сетапов: 101. Страница 2 из 2")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Результаты запуска 8" }));
    await screen.findByText("Всего сетапов: 101. Страница 1 из 2");
    expect(detailRequests).toContain("/v1/replay-runs/8?offset=0&limit=100");
    await userEvent.click(screen.getByRole("button", { name: "Скачать CSV запуска" }));
    await waitFor(() => expect(click).toHaveBeenCalledOnce());
    expect(requests.some((request) => request.path === "/v1/replay-runs/8/export.csv")).toBe(true);
    expect(createObjectURL).toHaveBeenCalledOnce();
    view.unmount();
    expect(revokeObjectURL).toHaveBeenCalledWith("blob:test-run");
  });
  it("shows only reported live instrument states and bounds the status table", async () => {
    const original = globalThis.fetch;
    vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => path === "/v1/live/status" ? Promise.resolve(new Response(JSON.stringify({ enabled: false, running: false, lastScanAt: null, lastError: null, instruments: [
      { symbol: "READY", state: "ready", lastClosedUnix: 100 }, { symbol: "WAIT", state: "waiting_data" }, { symbol: "SHORT", state: "insufficient_history" }, { symbol: "OLD", state: "stale" }, { symbol: "FAIL", state: "error", error: "source unavailable" },
      ...Array.from({ length: 996 }, (_, index) => ({ symbol: `ITEM${index}`, state: "ready" })),
    ] }))) : original(path, init)));
    render(<SettingsPage />);
    await userEvent.click(screen.getByRole("tab", { name: "Сканирование" }));
    await userEvent.click(await screen.findByText("Состояния live-инструментов (1001)"));
    expect(screen.getByText("Ожидание данных")).toBeVisible();
    expect(screen.getByText("Недостаточно истории")).toBeVisible();
    expect(screen.getByText("Устаревшие данные")).toBeVisible();
    expect(screen.getByText("source unavailable")).toBeVisible();
    expect(screen.getByText("ITEM994")).toBeInTheDocument();
    expect(screen.queryByText("ITEM995")).not.toBeInTheDocument();
    expect(requests.some((request) => request.method === "PUT")).toBe(false);
  });

  it("shows a multi-instrument launch as one row with unified detail and batch cancel", async () => {
    replayRunsResponse = { runs: [{ id: 7, name: "Пакетный прогон", isBatch: true, symbol: "2 инструментов", status: "running", cacheHit: false, setupCount: 3, error: null, algorithmId: "price_volume_spike", algorithmVersion: "1.1.1", fromUnix: 100, toUnix: 200, createdAt: "2026-01-01T00:00:00.000Z", instrumentCount: 2, doneInstruments: 1, activeInstruments: 1, failedInstruments: 0 }] };
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    expect(await screen.findByText("2 инструментов")).toBeInTheDocument();
    expect(screen.getByText(/Выполняется \(1\/2\)/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Результаты запуска 7" }));
    await waitFor(() => expect(screen.getByText(/Всего сетапов: 1/)).toBeInTheDocument());
    expect(screen.getByText("Пакетный прогон · 2 инструментов · price_volume_spike · 1.1.1 · running")).toBeVisible();
    expect(requests.some((request) => request.path === "/v1/replay-batches/7?offset=0&limit=100")).toBe(true);
    await userEvent.click(screen.getByRole("button", { name: "Отменить запуск 7" }));
    await waitFor(() => expect(requests.some((request) => request.path === "/v1/replay-batches/7/cancel" && request.method === "POST")).toBe(true));
    await screen.findByText(/Отмена запуска #7 запрошена для всех инструментов/);
  });
});
