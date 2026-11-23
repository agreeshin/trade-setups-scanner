import { cleanup, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router";
import { RunsPage } from "./runs-page";

const run = { id: 7, name: "Сохранённое имя", symbol: "BTCUSDT", status: "done", cacheHit: false, setupCount: 0, error: null, algorithmId: "price_volume_spike", algorithmVersion: "1.1.1", fromUnix: 100, toUnix: 200, createdAt: "2026-01-01T00:00:00Z", params: {} };
let listedRun: Record<string, unknown>;
const requests: { path: string; method: string }[] = [];
beforeEach(() => {
  requests.length = 0;
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    requests.push({ path, method: init?.method ?? "GET" });
    const data = path === "/v1/replay-runs" ? { runs: [listedRun] }
      : path === "/v1/algorithms" ? { algorithms: [] }
      : path === "/v1/instruments" ? { instruments: [] } : { value: {} };
    return Promise.resolve(new Response(JSON.stringify(data)));
  }));
});
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

it.each([
  { isBatch: false, status: "running" }, { isBatch: true, status: "running" },
  { isBatch: false, status: "done" }, { isBatch: true, status: "done" },
])("renders only structured original research names without editing or persisting them (batch=$isBatch, status=$status)", async ({ isBatch, status }) => {
  listedRun = { ...run, isBatch, status, origin: "continuous", continuousStudyId: 41, continuousGenerationId: 999 };
  render(<MemoryRouter><RunsPage /></MemoryRouter>);
  const table = await screen.findByRole("table");
  const row = within(table).getAllByRole("row")[1]!;
  const cells = within(row).getAllByRole("cell");
  expect(cells[1]).toHaveTextContent(/^Исследование #41$/);
  expect(within(row).queryByText(run.name)).not.toBeInTheDocument();
  expect(within(row).queryByRole("button", { name: "Переименовать запуск 7" })).not.toBeInTheDocument();
  expect(within(row).getByRole("button", { name: "Результаты запуска 7" })).toBeInTheDocument();
  expect(within(row).getByRole("button", { name: `${status === "running" ? "Отменить" : "Удалить"} запуск 7` })).toBeInTheDocument();
  expect(listedRun.name).toBe(run.name);
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it.each([
  { origin: "manual", actualStudyId: 41, actualRevision: 9 },
  { origin: "study_actual", actualStudyId: 41, actualRevision: 9, continuousStudyId: 55 },
  { origin: "continuous", continuousGenerationId: 41 },
  { origin: "continuous", continuousStudyId: null, continuousGenerationId: 41 },
  { origin: "continuous", continuousStudyId: 0 },
  { origin: "continuous", continuousStudyId: -1 },
  { origin: "continuous", continuousStudyId: 1.5 },
  { origin: undefined, actualStudyId: 41 },
])("preserves an editable saved name without valid original provenance: %j", async (provenance) => {
  listedRun = { ...run, name: "Исследование #99", ...provenance };
  render(<MemoryRouter><RunsPage /></MemoryRouter>);
  const table = await screen.findByRole("table");
  const row = within(table).getAllByRole("row")[1]!;
  expect(within(row).getByTitle("Исследование #99")).toHaveTextContent("Исследование #99");
  expect(within(row).getByRole("button", { name: "Переименовать запуск 7" })).toBeInTheDocument();
  expect(within(row).queryByText("После исследования")).not.toBeInTheDocument();
  expect(within(row).queryByText(/ревизия/)).not.toBeInTheDocument();
  expect(within(row).getAllByRole("cell")[0]).toHaveTextContent(/^#7$/);
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});
