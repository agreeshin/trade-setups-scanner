import { afterEach, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HistoryPage } from "./history-page";

afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

const catalog = {
  instruments: [
    { id: 1, exchange: "bybit", category: "linear", symbol: "BTCUSDT", status: "active" },
    { id: 2, exchange: "bybit", category: "linear", symbol: "ETHUSDT", status: "active" },
    { id: 3, exchange: "bybit", category: "linear", symbol: "OLDUSDT", status: "delisted" },
  ],
};
const emptyCounts = { queued: 0, running: 0, done: 0, cancel_requested: 0, cancelled: 0, failed: 0 };
const planFixture = { id: 5, fromUnix: 1767225600, toUnix: 1767312000, status: "active", counts: { total: 2, pending: 1, enqueued: 1, failed: 0, cancelled: 0 }, activeJobs: 1, createdAt: "2026-01-01T00:00:00Z", updatedAt: "2026-01-01T00:00:00Z" };
const coverageSummary = {
  instruments: [
    { instrumentId: 1, exchange: "bybit", category: "linear", symbol: "BTCUSDT", status: "active", firstMinuteUnix: 1767225600, lastMinuteUnix: 1767312000, totalCandles: 1440, missingMinutes: 5 },
    { instrumentId: 2, exchange: "bybit", category: "linear", symbol: "ETHUSDT", status: "active", firstMinuteUnix: null, lastMinuteUnix: null, totalCandles: 0, missingMinutes: 0 },
    { instrumentId: 3, exchange: "bybit", category: "linear", symbol: "OLDUSDT", status: "delisted", firstMinuteUnix: null, lastMinuteUnix: null, totalCandles: 0, missingMinutes: 0 },
  ],
};

function stubFetch(options: { counts?: typeof emptyCounts; plans?: typeof planFixture[] } = {}) {
  const posts: { url: string; body: unknown }[] = [];
  vi.stubGlobal("fetch", vi.fn((url: string, init?: RequestInit) => {
    const path = url.replace(/^https?:\/\/[^/]+/, "");
    if (init?.method === "POST") {
      posts.push({ url: path, body: typeof init.body === "string" ? (JSON.parse(init.body) as unknown) : null });
      if (path === "/v1/history/backfill-plans") return Promise.resolve(new Response(JSON.stringify({ plan: planFixture }), { status: 202 }));
      return Promise.resolve(new Response(JSON.stringify({ jobs: [], created: 2, declined: 0 }), { status: 202 }));
    }
    if (path === "/v1/instruments") return Promise.resolve(new Response(JSON.stringify(catalog), { status: 200 }));
    if (path === "/v1/history/instruments/coverage-summary") return Promise.resolve(new Response(JSON.stringify(coverageSummary), { status: 200 }));
    if (path === "/v1/history/instruments/listing") return Promise.resolve(new Response(JSON.stringify({ instruments: catalog.instruments.slice(0, 2) }), { status: 200 }));
    if (path === "/v1/history/backfill-jobs") return Promise.resolve(new Response(JSON.stringify({ jobs: [], nextBeforeId: null, counts: options.counts ?? emptyCounts }), { status: 200 }));
    if (path === "/v1/history/backfill-plans") return Promise.resolve(new Response(JSON.stringify({ plans: options.plans ?? [] }), { status: 200 }));
    if (path === "/v1/history/collection-targets") return Promise.resolve(new Response(JSON.stringify({ targets: [] }), { status: 200 }));
    return Promise.resolve(new Response(JSON.stringify({}), { status: 200 }));
  }));
  return posts;
}

it("shows the server-side bulk load status that survives page reloads", async () => {
  stubFetch({ counts: { ...emptyCounts, queued: 2, running: 1 } });
  render(<HistoryPage />);
  expect(await screen.findByText(/Идёт массовая загрузка истории: активных заданий 3/)).toBeInTheDocument();
  expect(screen.getByText(/в очереди 2, выполняется 1/)).toBeInTheDocument();
});

it("reports absence of user bulk loads separately from routine collection", async () => {
  stubFetch();
  render(<HistoryPage />);
  expect(await screen.findByText(/Массовых загрузок, запущенных пользователем, сейчас нет/)).toBeInTheDocument();
});

it("imports the bybit listing, keeps the list editable and submits one confirmed batch", async () => {
  const posts = stubFetch();
  render(<HistoryPage />);
  await screen.findByText(/Выбрано инструментов: 0/);
  await userEvent.click(screen.getByRole("button", { name: "Импортировать текущий листинг Bybit (линейные USDT perpetual)" }));
  expect(await screen.findByText(/Импортирован текущий листинг Bybit: 2/)).toBeInTheDocument();
  expect(await screen.findByText(/Выбрано инструментов: 2/)).toBeInTheDocument();
  fireEvent.change(screen.getByLabelText("Начало загрузки"), { target: { value: "2026-01-01T00:00" } });
  fireEvent.change(screen.getByLabelText("Конец загрузки"), { target: { value: "2026-01-02T00:00" } });
  await userEvent.click(screen.getByRole("button", { name: "Создать план загрузки (2)" }));
  expect(await screen.findByText(/Создан план загрузки #5: 2 инструментов/)).toBeInTheDocument();
  expect(posts).toHaveLength(1);
  const plan = posts[0] as { url: string; body: { instrumentIds: number[]; fromUnix: number; toUnix: number } };
  expect(plan.url).toBe("/v1/history/backfill-plans");
  expect(plan.body.instrumentIds).toEqual([1, 2]);
});

it("shows the coverage overview table with data-only default and symbol filter", async () => {
  stubFetch();
  render(<HistoryPage />);
  await userEvent.click(screen.getByRole("tab", { name: "Текущий сбор свечей" }));
  expect(await screen.findByRole("heading", { name: "Обзор покрытия истории" })).toBeInTheDocument();
  expect(screen.getByText(/Инструментов со свечами: 1 из 3/)).toBeInTheDocument();
  expect(screen.getByText("1440")).toBeInTheDocument();
  expect(screen.getByText("5")).toBeInTheDocument();
  expect(screen.queryByText("—")).not.toBeInTheDocument();
  fireEvent.click(screen.getByLabelText("Только инструменты со свечами"));
  expect(await screen.findAllByText("—")).toHaveLength(2);
  fireEvent.change(screen.getByLabelText("Фильтр по символу"), { target: { value: "zzz" } });
  expect(await screen.findByText("Нет инструментов по текущему фильтру.")).toBeInTheDocument();
});

it("resets the time to UTC midnight of the newly picked date", () => {
  stubFetch();
  render(<HistoryPage />);
  const from = screen.getByLabelText("Начало загрузки");
  const local = (date: Date) => {
    const pad = (part: number) => String(part).padStart(2, "0");
    return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`;
  };
  fireEvent.change(from, { target: { value: "2026-01-30T14:30" } });
  expect(from).toHaveValue(local(new Date(Date.UTC(2026, 0, 30))));
  fireEvent.change(from, { target: { value: "2026-01-30T09:15" } });
  expect(from).toHaveValue("2026-01-30T09:15");
  fireEvent.change(from, { target: { value: "2026-02-01T09:15" } });
  expect(from).toHaveValue(local(new Date(Date.UTC(2026, 1, 1))));
});

it("lists active plans with pending items and cancels a plan", async () => {
  const posts = stubFetch({ plans: [planFixture] });
  render(<HistoryPage />);
  expect(await screen.findByText(/Активных планов загрузки: 1/)).toBeInTheDocument();
  expect(screen.getByText(/Инструментов ожидают освобождения лимитов очереди: 1/)).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "Отменить план 5" }));
  expect(await screen.findByText(/Отмена плана #5 запрошена/)).toBeInTheDocument();
  expect(posts.some((post) => post.url === "/v1/history/backfill-plans/5/cancel")).toBe(true);
});

it("groups long lists into separate tabs", async () => {
  stubFetch();
  render(<HistoryPage />);
  expect(await screen.findByRole("heading", { name: "Загрузить историю" })).toBeInTheDocument();
  expect(screen.queryByRole("heading", { name: "Текущий сбор свечей" })).not.toBeInTheDocument();
  await userEvent.click(screen.getByRole("tab", { name: "Текущий сбор свечей" }));
  expect(await screen.findByRole("heading", { name: "Текущий сбор свечей" })).toBeInTheDocument();
  expect(screen.getByText("Выберите инструмент для управления сбором.")).toBeInTheDocument();
  await userEvent.click(screen.getByRole("tab", { name: "Задания загрузки" }));
  expect(await screen.findByText("Заданий загрузки пока нет.")).toBeInTheDocument();
});
