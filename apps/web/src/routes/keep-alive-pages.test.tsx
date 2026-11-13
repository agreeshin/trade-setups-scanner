import { beforeEach, afterEach, describe, expect, it, vi } from "vitest";
import { useState } from "react";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { Provider } from "react-redux";
import { AppRoutes } from "../app-routes";
import { createStore } from "../store";
import { RouteActiveContext } from "./route-active";
import { usePlatformPoll } from "./use-platform-poll";

const requests: string[] = [];

beforeEach(() => {
  requests.length = 0;
  vi.stubGlobal("fetch", vi.fn((path: string) => {
    requests.push(path);
    let data: unknown = {};
    if (path === "/v1/status") data = { platform: "ready", marketData: "ready" };
    if (path === "/v1/settings/ui") data = { value: {} };
    if (path === "/v1/algorithms") data = { algorithms: [{
      id: "price_volume_spike", name: "Рост цены и объёма", version: "1", description: "Общие настройки", enabled: true,
      params: { priceGrowthPct: "12" },
      fields: [{ key: "priceGrowthPct", label: "Рост цены", type: "decimal", min: 0, max: 100, unit: "%" }],
    }] };
    if (path.startsWith("/v1/setups?")) data = { setups: [], offset: 0, limit: 25, total: 0, snapshotId: 0 };
    return Promise.resolve(new Response(JSON.stringify(data), { status: 200 }));
  }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

function renderAppAt(initialPath: string) {
  return render(
    <Provider store={createStore()}>
      <MemoryRouter initialEntries={[initialPath]}>
        <AppRoutes />
      </MemoryRouter>
    </Provider>,
  );
}

describe("keep-alive navigation tabs", () => {
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
    const growth = await screen.findByRole("spinbutton", { name: "Рост цены (%)" });
    fireEvent.change(growth, { target: { value: "15" } });
    await user.click(screen.getByRole("checkbox", { name: "Включён для live-сканирования" }));

    await user.click(navigation.getByRole("link", { name: "Обзор" }));
    expect(view.container.querySelector('[data-page-path="/algorithms"]')).toHaveAttribute("hidden");
    expect(growth).not.toBeVisible();

    await user.click(algorithmsLink);
    expect(view.container.querySelector('[data-page-path="/algorithms"]')).not.toHaveAttribute("hidden");
    expect(screen.getByRole("spinbutton", { name: "Рост цены (%)" })).toBe(growth);
    expect(growth).toHaveValue(15);
    expect(screen.getByRole("checkbox", { name: "Включён для live-сканирования" })).not.toBeChecked();
    expect(requests.filter((path) => path === "/v1/algorithms")).toHaveLength(1);
  });

  it("keeps the setups filter and refreshes follow-live immediately when returning to its tab", async () => {
    const user = userEvent.setup();
    const view = renderAppAt("/setups");
    const filter = await screen.findByRole<HTMLInputElement>("textbox");
    await user.type(filter, "BTC");
    expect(filter.value).toBe("BTC");
    const setupsCallsBefore = requests.filter((path) => path.startsWith("/v1/setups?")).length;

    await user.click(screen.getByRole("link", { name: "Обзор" }));
    expect(view.container.querySelector('[data-page-path="/setups"]')).toHaveAttribute("hidden");
    expect(view.container.querySelector('[data-page-path="/"]')).not.toHaveAttribute("hidden");
    expect(view.container.querySelector('[data-page-path="/"] [data-slot="card"]')).not.toHaveClass("max-w-2xl");

    await user.click(screen.getByRole("link", { name: "Сетапы" }));
    expect(view.container.querySelector('[data-page-path="/setups"]')).not.toHaveAttribute("hidden");
    const filterAfter = screen.getByRole<HTMLInputElement>("textbox");
    expect(filterAfter.value).toBe("BTC");
    const setupsCallsAfter = requests.filter((path) => path.startsWith("/v1/setups?")).length;
    expect(setupsCallsAfter).toBe(setupsCallsBefore + 1);
    expect(requests.filter((path) => path.startsWith("/v1/setups?")).at(-1)).toBe("/v1/setups?offset=0&limit=25&q=BTC");
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
    render(<PollHarness />);
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
  });
});
