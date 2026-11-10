import { beforeEach, afterEach, describe, expect, it, vi } from "vitest";
import { useState } from "react";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
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
  it("keeps page state and skips refetch when switching tabs and back", async () => {
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
    expect(setupsCallsAfter).toBe(setupsCallsBefore);
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
