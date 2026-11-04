import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
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
const requests: { path: string; method: string; body: unknown }[] = [];
beforeEach(() => {
  requests.length = 0;
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    requests.push({ path, method, body: typeof init?.body === "string" ? JSON.parse(init.body) as unknown : null });
    let data: unknown = {};
    if (path === "/v1/algorithms") data = { algorithms: [algorithm] };
    if (path === "/v1/instruments") data = { instruments };
    if (path === "/v1/instrument-lists/live") data = { configured: false, instruments: [] };
    if (path === "/v1/history/backfill-jobs") data = { jobs: [{ id: 9, instrumentId: 1, fromUnix: 100, toUnix: 200, checkpointUnix: 150, status: "running", error: null }] };
    if (path === "/v1/history/collection-targets") data = { targets: [] };
    if (path === "/v1/history/instruments/1/coverage") data = { instrumentId: 1, firstMinuteUnix: null, lastMinuteUnix: null, totalCandles: 0, intervals: [], gaps: [] };
    if (path === "/v1/settings/ui") data = { value: { existing: "keep" } };
    if (path === "/v1/replay-runs") data = { runs: [] };
    if (path === "/v1/settings/notifications") data = { maxAgeSeconds: 60 };
    if (path === "/v1/live/status") data = { enabled: false, running: false, lastScanAt: null, lastError: null };
    if (path === "/v1/replay-cache/stats") data = { entries: 3, bytes: 1024 };
    if (path === "/v1/replay-cache") data = { removed: 3 };
    if (path === "/v1/setups") data = { setups: Array.from({ length: 30 }, (_, index) => ({ symbol: `BTC${index}`, exchange: "bybit", algorithmId: "price_volume_spike", openTimeUnix: 100, closeTimeUnix: 200 + index, closePrice: "100" })) };
    return Promise.resolve(new Response(JSON.stringify(data), { status: 200 }));
  }));
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

describe("platform screens", () => {
  it("submits algorithm and local dates without overriding saved algorithm params", async () => {
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await waitFor(() => expect(screen.getByRole("button", { name: "Запустить" })).toBeEnabled());
    expect(screen.getByRole("checkbox", { name: "BTCUSDT (Trading)" })).toBeChecked();
    expect(screen.getByRole("checkbox", { name: "ETHUSDT (Delisted)" })).toBeChecked();
    fireEvent.change(screen.getByLabelText("Начало"), { target: { value: "2026-10-10T12:00" } });
    fireEvent.change(screen.getByLabelText("Конец"), { target: { value: "2026-10-10T13:00" } });
    await userEvent.click(screen.getByRole("button", { name: "Запустить" }));
    await screen.findByText(/Запрос запуска принят/);
    expect(requests.find((request) => request.method === "POST")?.body).toEqual({ async: true, algorithmId: "price_volume_spike", instruments: [{ exchange: "bybit", category: "linear", symbol: "BTCUSDT" }, { exchange: "bybit", category: "linear", symbol: "ETHUSDT" }], fromUnix: new Date("2026-10-10T12:00").getTime() / 1000, toUnix: new Date("2026-10-10T13:00").getTime() / 1000 });
    expect(requests.some((request) => request.method === "PUT")).toBe(false);
    await userEvent.click(screen.getByRole("button", { name: "Сохранить форму" }));
    await screen.findByText("Форма сохранена в базе сигналов.");
    expect(requests.find((request) => request.path === "/v1/settings/ui" && request.method === "PUT")?.body).toMatchObject({ value: { existing: "keep", replay: { algorithmId: "price_volume_spike" } } });
  });
  it("blocks a reversed date range before POST", async () => {
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await waitFor(() => expect(screen.getByRole("button", { name: "Запустить" })).toBeEnabled());
    fireEvent.change(screen.getByLabelText("Начало"), { target: { value: "2026-10-10T13:00" } });
    fireEvent.change(screen.getByLabelText("Конец"), { target: { value: "2026-10-10T12:00" } });
    await userEvent.click(screen.getByRole("button", { name: "Запустить" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("конец диапазона должен быть позже начала");
    expect(requests.some((request) => request.method === "POST")).toBe(false);
  });
  it("keeps percent and minute units distinct and requires cache confirmation", async () => {
    render(<SettingsPage />);
    expect(screen.getByText(/Telegram не подключён/)).toBeInTheDocument();
    expect(screen.getByText(/Web Push недоступен по обычному HTTP/)).toHaveTextContent("localhost");
    const growth = await screen.findByLabelText("Рост цены (%)");
    expect(growth).toHaveValue(12);
    expect(screen.getByLabelText("Окно (мин)")).toHaveValue(5);
    fireEvent.change(growth, { target: { value: "15" } });
    await userEvent.click(screen.getByRole("button", { name: "Сохранить алгоритм" }));
    await screen.findByText("Настройки алгоритма сохранены.");
    expect(requests.find((request) => request.path === "/v1/algorithms/price_volume_spike")?.body).toEqual({ params: { priceGrowthPct: "15", windowMinutes: 5, volumeMultiplier: "3" }, enabled: true });
    expect(screen.getByRole("button", { name: "Очистить кеш" })).toBeDisabled();
    await userEvent.click(screen.getByLabelText("Подтверждаю очистку всего кеша"));
    await userEvent.click(screen.getByRole("button", { name: "Очистить кеш" }));
    await screen.findByText("Удалено записей кеша: 3.");
    expect(screen.getByRole("button", { name: "Очистить кеш" })).toBeDisabled();
    expect(requests.some((request) => request.path === "/v1/settings/live" && request.method === "PUT")).toBe(false);
  });
  it("filters and paginates the explicitly bounded latest feed", async () => {
    render(<SetupsPage />);
    await screen.findByText("Страница 1 из 2 · Найдено: 30");
    expect(screen.queryByText("bybit · BTC29")).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Далее" }));
    expect(screen.getByText("bybit · BTC29")).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText("Инструмент, биржа или алгоритм"), { target: { value: "BTC29" } });
    expect(screen.getByText("Страница 1 из 1 · Найдено: 1")).toBeInTheDocument();
    expect(requests.some((request) => request.method === "PUT")).toBe(false);
  });
  it("reports unavailable algorithms instead of enabling a fabricated run", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("{}", { status: 503 }))));
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await screen.findByText("Алгоритмы недоступны (HTTP 503)");
    expect(screen.getByRole("button", { name: "Запустить" })).toBeDisabled();
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
    const run = { id: 7, symbol: "BTCUSDT", status: "completed", cacheHit: false, setupCount: 1, error: null, algorithmId: "price_volume_spike", algorithmVersion: "1", fromUnix: 100, toUnix: 300, params: { priceGrowthPct: "17", windowMinutes: 5 } };
    vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
      if (path === "/v1/replay-runs") return Promise.resolve(new Response(JSON.stringify({ runs: [run] })));
      if (path === "/v1/replay-runs/7?offset=0&limit=100") return Promise.resolve(new Response(JSON.stringify({ run, offset: 0, limit: 100, total: 1, setups: [{ exchange: "bybit", category: "linear", symbol: "BTCUSDT", algorithmId: "price_volume_spike", openTimeUnix: 100, closeTimeUnix: 200, closePrice: "100" }] })));
      return original(path, init);
    }));
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole("button", { name: "Результаты запуска 7" }));
    await userEvent.click(await screen.findByText("Снимок параметров запуска"));
    expect(screen.getByText("17")).toBeVisible();
    expect(screen.getByText("WATCH")).toBeVisible();
    expect(screen.getByRole("link", { name: "TradingView" })).toHaveAttribute("href", "https://www.tradingview.com/chart/?symbol=BYBIT%3ABTCUSDT.P");
    expect(screen.getByRole("link", { name: "Bybit" })).toHaveAttribute("rel", "noopener noreferrer");
  });
  it("loads history without enqueuing and only mutates on explicit actions", async () => {
    render(<HistoryPage />);
    await screen.findByRole("button", { name: "Отменить загрузку 9" });
    expect(requests.every((request) => request.method === "GET")).toBe(true);
    await userEvent.selectOptions(screen.getByLabelText("Инструмент БД"), "1");
    await screen.findByText("В БД нет свечей этого инструмента.");
    fireEvent.change(screen.getByLabelText("Начало загрузки"), { target: { value: "2026-10-10T12:00" } });
    fireEvent.change(screen.getByLabelText("Конец загрузки"), { target: { value: "2026-10-10T13:00" } });
    await userEvent.click(screen.getByRole("button", { name: "Поставить загрузку в очередь" }));
    await screen.findByText("Запрос загрузки истории принят. Проверяйте статус задания ниже.");
    expect(requests.find((request) => request.path === "/v1/history/backfill-jobs" && request.method === "POST")?.body).toMatchObject({ instrumentId: 1, fromUnix: new Date("2026-10-10T12:00").getTime() / 1000 });
    await userEvent.click(screen.getByRole("button", { name: "Отменить загрузку 9" }));
    await screen.findByText("Отмена задания #9 запрошена.");
    expect(requests.some((request) => request.path === "/v1/history/backfill-jobs/9/cancel" && request.method === "POST")).toBe(true);
    await userEvent.click(screen.getByRole("button", { name: "Включить сбор" }));
    await screen.findByText("Настройка сбора сохранена.");
    expect(requests.find((request) => request.path === "/v1/history/collection-targets/1")?.body).toEqual({ enabled: true });
  });
  it("distinguishes unavailable coverage from empty history", async () => {
    const original = globalThis.fetch;
    vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => path.includes("/coverage") ? Promise.resolve(new Response("{}", { status: 503 })) : original(path, init)));
    render(<HistoryPage />);
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
    vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => path === "/v1/replay-runs" ? Promise.resolve(new Response(JSON.stringify({ runs: [{ id: 12, symbol: "BTCUSDT", status: "queued", cacheHit: false, setupCount: 0, algorithmId: "price_volume_spike", algorithmVersion: "1" }] }))) : original(path, init)));
    render(<MemoryRouter><RunsPage /></MemoryRouter>);
    await userEvent.click(await screen.findByRole("button", { name: "Отменить запуск 12" }));
    await screen.findByText("Отмена запуска #12 запрошена. Дождитесь конечного статуса.");
    expect(requests.some((request) => request.path === "/v1/replay-runs/12/cancel" && request.method === "POST")).toBe(true);
  });
  it("pages run results server-side, preserves the page during refresh and downloads the complete CSV", async () => {
    const original = globalThis.fetch;
    const detailRequests: string[] = [];
    const run = { id: 7, symbol: "BTCUSDT", status: "completed", cacheHit: false, setupCount: 101, algorithmId: "price_volume_spike", algorithmVersion: "1", fromUnix: 100, toUnix: 300, params: {} };
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
    await userEvent.click(await screen.findByText("Состояния live-инструментов (1001)"));
    expect(screen.getByText("Ожидание данных")).toBeVisible();
    expect(screen.getByText("Недостаточно истории")).toBeVisible();
    expect(screen.getByText("Устаревшие данные")).toBeVisible();
    expect(screen.getByText("source unavailable")).toBeVisible();
    expect(screen.getByText("ITEM994")).toBeInTheDocument();
    expect(screen.queryByText("ITEM995")).not.toBeInTheDocument();
    expect(requests.some((request) => request.method === "PUT")).toBe(false);
  });
});
