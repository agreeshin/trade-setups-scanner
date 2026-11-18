import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { SetupsAnalyticsPanel } from "./setups-analytics-panel";

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });
const profile1 = {
  slotId: 1, studyId: 40, generationId: 36, setupCount: 3, direction: "short", meanR: 0.32,
  tpPct: 20, slPct: 6, timeShiftMinutes: 5, priceChangePct: 2, externalName: "Всплеск цены и оборота",
};
const profile2 = {
  slotId: 3, studyId: 41, generationId: 37, setupCount: 2, direction: "long", meanR: -0.1,
  tpPct: 15, slPct: 5, timeShiftMinutes: 10, priceChangePct: null, externalName: "Всплеск цены и оборота",
};
const setupRow = (id: number, slotId: number, status: string, riskReturn: number | null) => ({
  id, symbol: `SYM${id}`, exchange: "bybit", slotId, studyId: 40, direction: "short",
  setupTimeUnix: 1794000000, entryTimeUnix: 1794000300, entryPrice: 98.5, gateThreshold: 98,
  tpPrice: 78.8, slPrice: 104.41, closeTimeUnix: 1794003600, riskReturn, status,
});
const analyticsResult = {
  totals: { selected: 5, gatePassed: 3, closed: 2, meanR: 1.25 },
  studies: [{ studyId: 40, selected: 5, gatePassed: 3, closed: 2, meanR: 1.25 }],
  setups: [
    setupRow(1, 1, "tp", 3.33), setupRow(2, 1, "sl", -1), setupRow(3, 3, "unclosed", null),
    setupRow(4, 1, "no_data", null), setupRow(5, 3, "gate_failed", null),
  ],
};

const requests: string[] = [];
let analyticsResponse: () => Response = () => json(analyticsResult);

beforeEach(() => {
  vi.useFakeTimers(); vi.setSystemTime(new Date("2026-11-13T12:00:00Z")); requests.length = 0;
  analyticsResponse = () => json(analyticsResult);
  vi.stubGlobal("fetch", vi.fn((path: string) => {
    requests.push(path);
    if (path === "/v1/algorithms") return Promise.resolve(json({ algorithms: [{ id: "price_volume_spike", name: "Рост цены и объёма", version: "1", description: "", enabled: true, params: {}, fields: [] }] }));
    if (path.startsWith("/v1/setups/analytics/profiles")) return Promise.resolve(json({ profiles: [profile1, profile2] }));
    if (path.startsWith("/v1/setups/analytics?")) return Promise.resolve(analyticsResponse());
    return Promise.resolve(json({}));
  }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

const settle = async () => { await act(async () => { await vi.advanceTimersByTimeAsync(0); }); };
const tick = async (ms = 400) => { await act(async () => { await vi.advanceTimersByTimeAsync(ms); }); };
const analyticsCalls = () => requests.filter((path) => path.startsWith("/v1/setups/analytics?"));

async function rendered() {
  render(<SetupsAnalyticsPanel />);
  await settle();
  await tick();
  return null;
}

it("loads algorithms and releaser profiles, then renders the calculated result", async () => {
  await rendered();
  expect(requests).toContain("/v1/algorithms");
  expect(requests.some((path) => path.startsWith("/v1/setups/analytics/profiles"))).toBe(true);
  expect(screen.getByRole("button", { name: "Профили: Все (2)" })).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  expect(analyticsCalls()).toHaveLength(1);
  const mean = screen.getByTestId("setups-analytics-mean");
  expect(mean.className).toContain("text-success");
  expect(screen.getAllByText("3/5")).toHaveLength(2);
  expect(screen.getByText("Исследование №40")).toBeInTheDocument();
  expect(screen.getByText("SYM1")).toBeInTheDocument();
  expect(screen.getAllByText("TP")).toHaveLength(2);
  expect(screen.getAllByText("SL")).toHaveLength(2);
  expect(screen.getByText("не закрылась")).toBeInTheDocument();
  expect(screen.getByText("нет данных")).toBeInTheDocument();
  expect(screen.getByText("гейт не пройден")).toBeInTheDocument();
  expect(screen.getAllByRole("button", { name: "№1" }).length).toBeGreaterThan(0);
});

it("opens the picker, edits the profile selection and passes slots to the calculation", async () => {
  await rendered();
  fireEvent.click(screen.getByRole("button", { name: "Профили: Все (2)" }));
  await settle();
  const dialog = screen.getByRole("dialog");
  expect(within(dialog).getByText("Профили, выпустившие сетапы")).toBeInTheDocument();
  expect(within(dialog).getByText("Выбрано: 2 из 2")).toBeInTheDocument();
  fireEvent.click(within(dialog).getByLabelText("Профиль 3"));
  expect(within(dialog).getByText("Выбрано: 1 из 2")).toBeInTheDocument();
  fireEvent.click(within(dialog).getByRole("button", { name: "Готово" }));
  await settle();
  expect(screen.getByRole("button", { name: "Профили: 1 из 2" })).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  const query = new URL(analyticsCalls().at(-1)!, "http://localhost").searchParams;
  expect(query.get("slots")).toBe("1");
  expect(query.get("algorithmId")).toBe("price_volume_spike");
});

it("shows profile details from a calculated row", async () => {
  await rendered();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  fireEvent.click(screen.getAllByRole("button", { name: "№1" })[0]!);
  await settle();
  const dialog = screen.getByRole("dialog");
  expect(within(dialog).getByText("Профиль №1")).toBeInTheDocument();
  expect(within(dialog).getByText("Исследование-основа")).toBeInTheDocument();
  expect(within(dialog).getByText("+0.32R")).toBeInTheDocument();
  expect(within(dialog).getByText(/5 мин/)).toBeInTheDocument();
  expect(within(dialog).getByText("2%")).toBeInTheDocument();
});

it("disables calculation and hints when the range is invalid", async () => {
  await rendered();
  fireEvent.change(screen.getByLabelText("Начало (момент выхода сетапа)"), { target: { value: "2026-11-20T00:00" } });
  await settle();
  expect(screen.getByRole("alert")).toHaveTextContent("конец диапазона должен быть позже начала");
  expect(screen.getByRole("button", { name: "Рассчитать" })).toBeDisabled();
  expect(analyticsCalls()).toHaveLength(0);
});

it("shows the limit error with a range hint", async () => {
  analyticsResponse = () => json({ error: "setups_analytics_limit_exceeded" }, 400);
  await rendered();
  fireEvent.click(screen.getByRole("button", { name: "Рассчитать" }));
  await settle();
  expect(screen.getByRole("alert")).toHaveTextContent("Сузьте диапазон");
});
