import { beforeEach, afterEach, describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import { StrictMode, useState } from "react";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { Provider } from "react-redux";
import { AppRoutes } from "../app-routes";
import { createStore } from "../store";
import { RouteActiveContext } from "./route-active";
import { usePlatformPoll } from "./use-platform-poll";
import { applyInterfaceSettings, DEFAULT_INTERFACE_SETTINGS } from "../lib/datetime-format";
import { actualContinuousFixture } from "../../test/study-actual-fixture";

const requests: string[] = [];
const mutations: string[] = [];
const refreshSignals: (AbortSignal | null | undefined)[] = [];
let refreshResponse: () => Response | Promise<Response>;

beforeEach(() => {
  requests.length = 0;
  mutations.length = 0;
  refreshSignals.length = 0;
  window.localStorage.clear();
  applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "UTC" });
  vi.spyOn(window, "scrollTo").mockImplementation(() => undefined);
  refreshResponse = () => new Response(JSON.stringify({ snapshotId: 0, observedThroughUnix: 200, processed: 0, computed: 0, updated: 0, nextCursor: null }));
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    requests.push(path);
    if (init?.method && init.method !== "GET") mutations.push(path);
    if (path === "/v1/setups/outcomes/refresh") { refreshSignals.push(init?.signal); return Promise.resolve(refreshResponse()); }
    let data: unknown = {};
    if (path === "/v1/status") data = { platform: "ready", marketData: "ready" };
    if (path === "/v1/settings/ui") data = { value: {} };
    if (path.startsWith("/v1/optimization-studies/actual/continuous?")) data = actualContinuousFixture(new URL(path, "http://localhost").searchParams.get("algorithmId")!);
    if (path === "/v1/algorithms") data = { algorithms: [{
      id: "price_volume_spike", name: "Рост цены и объёма", version: "1", description: "Общие настройки", enabled: true,
      params: { priceGrowthPct: "12" },
      fields: [{ key: "priceGrowthPct", label: "Рост цены", type: "decimal", min: 0, max: 100, unit: "%" }],
    }] };
    if (path.startsWith("/v1/setups?")) data = { setups: [], offset: 0, limit: 25, total: 0, snapshotId: 0 };
    if (path.startsWith("/v1/setups/analytics/studies?")) data = { studies: [{ studyId: 40, externalName: "Актуальное исследование", setupCount: 1, generationId: 36, artifactId: null }] };
    if (path.startsWith("/v1/setups/analytics?")) data = { totals: { selected: 0, gatePassed: 0, closed: 0, meanR: null }, studies: [], setups: [] };
    return Promise.resolve(new Response(JSON.stringify(data), { status: 200 }));
  }));
});
afterEach(() => { cleanup(); applyInterfaceSettings(DEFAULT_INTERFACE_SETTINGS); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

const analyticsPosts = () => mutations.filter((path) => path.startsWith("/v1/setups/analytics?"));

function renderAppAt(initialPath: string, strict = false) {
  const application = (
    <Provider store={createStore()}>
      <MemoryRouter initialEntries={[initialPath]}>
        <AppRoutes />
      </MemoryRouter>
    </Provider>
  );
  return render(strict ? <StrictMode>{application}</StrictMode> : application);
}

describe("keep-alive navigation tabs", () => {
  it("applies the Algorithms-only desktop width rule only while that retained page is visible", async () => {
    const selector = '[data-slot="app-main"]:has(> [data-slot="page-card"] [data-page-path="/algorithms"]:not([hidden]))';
    const globalStyles = readFileSync("src/styles/global.css", "utf8");
    expect(globalStyles).toContain('@media (width >= 48rem)');
    expect(globalStyles).toContain(`${selector} {\n    max-width: none;\n  }`);
    const scrollTo = vi.spyOn(window, "scrollTo").mockImplementation(() => undefined);
    const user = userEvent.setup(); renderAppAt("/");
    const main = screen.getByRole("main");
    expect(main.matches(selector)).toBe(false);
    await user.click(screen.getByRole("link", { name: "Алгоритмы" }));
    await screen.findByRole("tab", { name: "Рост цены и объёма" });
    expect(main.matches(selector)).toBe(true);
    expect(main).toHaveClass("px-4", "sm:px-6");
    expect(main.querySelector('[data-slot="page-card"]')).toHaveAttribute("class", "min-w-0");
    expect(main.querySelector('[data-slot="page-sidebar"]')).toHaveClass("md:right-0", "md:fixed", "md:w-56");
    await user.click(screen.getByRole("link", { name: "Обзор" }));
    expect(main.matches(selector)).toBe(false);
    expect(main.querySelector('[data-page-path="/algorithms"]')).toHaveAttribute("hidden");
    expect(main).toHaveClass("max-w-7xl");
    const sidebarSelector = '[data-slot="app-main"]:has([data-page-path]:not([hidden]) [data-slot="page-sidebar"])';
    expect(main.matches(sidebarSelector)).toBe(false);
    expect(scrollTo).toHaveBeenCalled();
  });

  it("preserves one StrictMode refresh across navigation, then calculates once on the first active ready visit", async () => {
    let finish: ((response: Response) => void) | undefined;
    refreshResponse = () => new Promise<Response>((resolve) => { finish = resolve; });
    const user = userEvent.setup();
    const view = renderAppAt("/setups", true);
    const icon = await screen.findByRole("button", { name: "Обновить результаты сетапов" });
    expect(icon).toBeDisabled(); expect(refreshSignals).toHaveLength(1);
    expect(refreshSignals[0]?.aborted).toBe(false);
    expect(screen.getByRole("status", { name: "Обновление результатов сделок" })).toBeVisible();
    expect(screen.queryByRole("tab", { name: "Лента" })).not.toBeInTheDocument();
    expect(screen.queryByRole("tab", { name: "Аналитика" })).not.toBeInTheDocument();
    expect(analyticsPosts()).toHaveLength(0);
    expect(requests.filter((path) => path.startsWith("/v1/setups/analytics/studies?"))).toHaveLength(0);
    await user.click(screen.getByRole("link", { name: "Обзор" }));
    expect(refreshSignals[0]?.aborted).toBe(false);
    await act(async () => { finish?.(new Response(JSON.stringify({ snapshotId: 0, observedThroughUnix: 200, processed: 1, computed: 1, updated: 1, nextCursor: null }))); await Promise.resolve(); });
    expect(analyticsPosts()).toHaveLength(0);
    await user.click(screen.getByRole("link", { name: "Сетапы" }));
    await screen.findByTestId("setups-analytics-mean");
    expect(screen.getByRole("button", { name: "Обновить результаты сетапов" })).toBe(icon);
    expect(icon).toBeEnabled(); expect(icon).toHaveAttribute("aria-busy", "false");
    expect(screen.getByText(/Результаты обновлены:/)).toHaveTextContent("обновлено 1");
    await user.click(screen.getByRole("link", { name: "Обзор" }));
    await user.click(screen.getByRole("link", { name: "Сетапы" }));
    expect(refreshSignals).toHaveLength(1); expect(refreshSignals[0]?.aborted).toBe(false);
    expect(analyticsPosts()).toHaveLength(1);
    const refreshIndex = requests.indexOf("/v1/setups/outcomes/refresh");
    const optionsIndex = requests.findIndex((path) => path.startsWith("/v1/setups/analytics/studies?"));
    const calculateIndex = requests.findIndex((path) => path.startsWith("/v1/setups/analytics?"));
    expect(refreshIndex).toBeLessThan(optionsIndex); expect(optionsIndex).toBeLessThan(calculateIndex);
    view.unmount();
  });

  it("uses the full-application boot second for End rather than saved End or the later first visit", async () => {
    const boot = Date.parse("2026-11-21T12:34:56Z") / 1000;
    const clock = vi.spyOn(Date, "now").mockReturnValue(boot * 1000);
    window.localStorage.setItem("setups-analytics-filter-v1", JSON.stringify({ algorithmId: "price_volume_spike", from: "2026-11-14T00:00", to: "2026-11-15T00:00", studies: [999] }));
    const user = userEvent.setup(); renderAppAt("/");
    await screen.findByText("Сервис данных: готов");
    clock.mockReturnValue((boot + 7200) * 1000);
    await user.click(screen.getByRole("link", { name: "Сетапы" }));
    await screen.findByTestId("setups-analytics-mean");
    expect(screen.getByLabelText("Конец")).toHaveValue("2026-11-21T12:34");
    expect(screen.getByLabelText("Начало (момент выхода сетапа)")).toHaveValue("2026-11-14T00:00");
    const calculation = new URL(analyticsPosts()[0]!, "http://localhost").searchParams;
    expect(calculation.get("toUnix")).toBe(String(boot));
    expect(calculation.has("studies")).toBe(false);
    const options = requests.filter((path) => path.startsWith("/v1/setups/analytics/studies?"));
    expect(options).toHaveLength(1);
    expect(new URL(options[0]!, "http://localhost").searchParams.get("toUnix")).toBe(String(boot));
    await user.click(screen.getByRole("link", { name: "Обзор" }));
    clock.mockReturnValue((boot + 10800) * 1000);
    await user.click(screen.getByRole("link", { name: "Сетапы" }));
    expect(screen.getByLabelText("Конец")).toHaveValue("2026-11-21T12:34");
    expect(analyticsPosts()).toHaveLength(1); expect(refreshSignals).toHaveLength(1);
  });

  it("mounts algorithms on first visit and preserves drafts without reloading on return", async () => {
    const user = userEvent.setup();
    const view = renderAppAt("/");
    const navigation = within(screen.getByRole("navigation", { name: "Основная навигация" }));
    expect(navigation.getAllByRole("link").map((link) => link.textContent)).toEqual(["Обзор", "Сетапы", "Аналитика", "Запуски", "Алгоритмы", "Настройки", "История и сбор"]);
    const algorithmsLink = navigation.getByRole("link", { name: "Алгоритмы" });
    expect(algorithmsLink).toHaveAttribute("href", "/algorithms");
    expect(requests.filter((path) => path === "/v1/algorithms")).toHaveLength(0);

    await user.click(algorithmsLink);
    expect(screen.getByRole("heading", { level: 1, name: "Алгоритмы" })).toHaveClass("sr-only");
    expect(algorithmsLink).toHaveAttribute("aria-current", "page");
    const fixed = await screen.findByRole("tab", { name: "Live-сканирование с постоянными параметрами" });
    const liveTabs = screen.getByRole("tablist", { name: "Режим live · Рост цены и объёма" });
    expect(liveTabs).toHaveAttribute("data-slot", "tabs-list");
    expect(liveTabs).toHaveClass("min-w-0", "max-w-full", "overflow-x-auto");
    expect(liveTabs).toHaveClass("w-full", "justify-start", "sm:w-fit", "sm:justify-center-safe", "sm:justify-self-center");
    expect(fixed).toHaveAttribute("data-slot", "tabs-trigger");
    liveTabs.scrollLeft = 90;
    await user.click(fixed);
    const growth = await screen.findByRole("spinbutton", { name: "Рост цены (%)" });
    const fixedPanel = growth.closest('[data-slot="tabs-content"]')!;
    fixedPanel.scrollTop = 240;
    fireEvent.change(growth, { target: { value: "15" } });
    await user.click(screen.getByRole("checkbox", { name: "Включён для live-сканирования" }));
    await user.click(screen.getByRole("tab", { name: "Live-сканирование с гибкими параметрами" }));
    expect(growth).toBeInTheDocument(); expect(growth).not.toBeVisible();
    expect(fixedPanel).toHaveAttribute("hidden");

    await user.click(navigation.getByRole("link", { name: "Обзор" }));
    expect(view.container.querySelector('[data-page-path="/algorithms"]')).toHaveAttribute("hidden");
    expect(growth).not.toBeVisible();

    await user.click(algorithmsLink);
    expect(view.container.querySelector('[data-page-path="/algorithms"]')).not.toHaveAttribute("hidden");
    expect(screen.getByRole("tab", { name: "Live-сканирование с гибкими параметрами" })).toHaveAttribute("data-state", "active");
    expect(screen.getByRole("tablist", { name: "Режим live · Рост цены и объёма" })).toBe(liveTabs);
    expect(liveTabs.scrollLeft).toBe(90);
    await user.click(fixed);
    expect(screen.getByRole("spinbutton", { name: "Рост цены (%)" })).toBe(growth);
    expect(growth.closest('[data-slot="tabs-content"]')).toBe(fixedPanel);
    expect(fixedPanel.scrollTop).toBe(240);
    expect(growth).toHaveValue(15);
    expect(screen.getByRole("checkbox", { name: "Включён для live-сканирования" })).not.toBeChecked();
    expect(requests.filter((path) => path === "/v1/algorithms")).toHaveLength(1);
    expect(mutations).toHaveLength(0);
  });

  it("retains Setups filters and fixed End across navigation without new refresh or calculation commands", async () => {
    const user = userEvent.setup();
    const view = renderAppAt("/setups");
    const filter = await screen.findByRole<HTMLInputElement>("textbox", { name: "Инструмент, биржа или алгоритм" });
    await screen.findByTestId("setups-analytics-mean");
    const end = screen.getByLabelText("Конец");
    const endValue = (end as HTMLInputElement).value;
    await user.type(filter, "BTC");
    expect(filter.value).toBe("BTC");
    await user.click(screen.getByRole("button", { name: "Рассчитать" }));
    await screen.findByTestId("setups-analytics-mean");
    expect(analyticsPosts()).toHaveLength(2);

    await user.click(screen.getByRole("link", { name: "Обзор" }));
    expect(view.container.querySelector('[data-page-path="/setups"]')).toHaveAttribute("hidden");
    expect(view.container.querySelector('[data-page-path="/"]')).not.toHaveAttribute("hidden");
    expect(view.container.querySelector('[data-page-path="/"] [data-slot="card"]')).not.toHaveClass("max-w-2xl");

    await user.click(screen.getByRole("link", { name: "Сетапы" }));
    expect(view.container.querySelector('[data-page-path="/setups"]')).not.toHaveAttribute("hidden");
    const filterAfter = screen.getByRole<HTMLInputElement>("textbox", { name: "Инструмент, биржа или алгоритм" });
    expect(filterAfter.value).toBe("BTC");
    expect(filterAfter).toBe(filter);
    expect(screen.getByLabelText("Конец")).toBe(end); expect(end).toHaveValue(endValue);
    expect(analyticsPosts()).toHaveLength(2); expect(refreshSignals).toHaveLength(1);
    expect(new URL(analyticsPosts()[1]!, "http://localhost").searchParams.get("q")).toBe("BTC");
    const latestHistory = new URL(requests.filter((path) => path.startsWith("/v1/setups?")).at(-1)!, "http://localhost").searchParams;
    expect(latestHistory.get("q")).toBe("BTC"); expect(latestHistory.get("snapshotId")).toBe("0");
    expect(screen.getByRole("link", { name: "Экспорт CSV (без фильтра)" })).toHaveAttribute("href", "/v1/export/setups.csv");
  });

  it("restores window scroll offset when returning to a tab", async () => {
    const scrollTo = vi.spyOn(window, "scrollTo").mockImplementation(() => undefined);
    vi.spyOn(window, "scrollY", "get").mockReturnValue(777);
    const user = userEvent.setup();
    renderAppAt("/");
    fireEvent.scroll(window);
    await user.click(screen.getByRole("link", { name: "Сетапы" }));
    await user.click(screen.getByRole("link", { name: "Обзор" }));
    expect(scrollTo).toHaveBeenCalledWith({ top: 777 });
  });
});

function PollTarget() {
  usePlatformPoll<{ ok: boolean }>("/v1/poll");
  return null;
}

function PollHarness() {
  const [active, setActive] = useState(true);
  return <>
    <RouteActiveContext.Provider value={active}>
      <PollTarget />
    </RouteActiveContext.Provider>
    <button onClick={() => setActive((value) => !value)}>toggle</button>
  </>;
}

describe("poll pause on hidden route", () => {
  it("stops polling while the route is inactive and resumes immediately on return", async () => {
    vi.useFakeTimers();
    const view = render(<PollHarness />);
    const pollCalls = () => requests.filter((path) => path === "/v1/poll").length;
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    expect(pollCalls()).toBe(1);
    await act(async () => { await vi.advanceTimersByTimeAsync(3000); });
    expect(pollCalls()).toBe(2);
    act(() => { fireEvent.click(screen.getByRole("button", { name: "toggle" })); });
    await act(async () => { await vi.advanceTimersByTimeAsync(9000); });
    expect(pollCalls()).toBe(2);
    act(() => { fireEvent.click(screen.getByRole("button", { name: "toggle" })); });
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    expect(pollCalls()).toBe(3);
    view.unmount(); await act(async () => { await vi.advanceTimersByTimeAsync(9000); });
    expect(pollCalls()).toBe(3); expect(vi.getTimerCount()).toBe(0);
  });
});
