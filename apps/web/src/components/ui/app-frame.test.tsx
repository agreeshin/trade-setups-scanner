import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { readFileSync } from "node:fs";
import { MemoryRouter, Route, Routes } from "react-router";
import { afterEach, expect, it, vi } from "vitest";
import { AppLayout, useApplicationLoad } from "../../routes/app-layout";
import { AppFrame } from "./app-frame";
import { Card } from "./card";
import { PageSidebar } from "./page-sidebar";

afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("removes only the outer card visual and its second padding, retaining nested cards and edge navigation", () => {
  const { container } = render(<AppFrame navigation="Меню" status="Готов"><PageSidebar label="Разделы" items={[{ id: "one", label: "Первый" }]} value="one" onValueChange={vi.fn()}><Card>Центральная зона</Card></PageSidebar></AppFrame>);
  expect(container.querySelector('[data-slot="page-card"]')).toHaveAttribute("class", "min-w-0");
  expect(container.querySelector('[data-slot="card"]')).toHaveClass("rounded-xl", "bg-muted/40", "p-5");
  expect(container.querySelector('[data-slot="page-sidebar"]')).toHaveClass("rounded-none", "md:right-0", "md:fixed");
  expect(screen.getByRole("main")).toHaveAttribute("data-slot", "app-main");
  expect(screen.getByRole("main")).toHaveClass("max-w-7xl", "px-4", "sm:px-6", "min-w-0");
  const styles = readFileSync("src/styles/global.css", "utf8");
  expect(styles).toContain("max-width: calc(80rem + 14rem);");
  expect(styles).toContain("padding-right: calc(14rem + 1.5rem);");
});

it("measures a wrapping header and releases its sole observer on unmount", () => {
  const disconnect = vi.fn();
  const observe = vi.fn();
  vi.stubGlobal("ResizeObserver", class { observe = observe; disconnect = disconnect; });
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockReturnValue({ height: 112 } as DOMRect);
  const view = render(<AppFrame navigation="Меню" status="Готов">Страница</AppFrame>);
  expect(view.container.querySelector('[data-slot="app-frame"]')).toHaveStyle({ "--app-header-height": "112px" });
  expect(observe).toHaveBeenCalledWith(screen.getByRole("banner"));
  view.unmount();
  expect(disconnect).toHaveBeenCalledOnce();
});

it("scopes the 1920px Setups card limit to the active retained page", () => {
  const selector = '[data-slot="app-main"]:has(> [data-slot="page-card"] [data-page-path="/setups"]:not([hidden]))';
  expect(readFileSync("src/styles/global.css", "utf8")).toContain(`${selector} {\n    max-width: calc(1920px + 3rem);\n  }`);
  const view = render(<AppFrame navigation="Меню" status="Готов"><div data-page-path="/setups">Список</div></AppFrame>);
  expect(screen.getByRole("main").matches(selector)).toBe(true);
  view.rerender(<AppFrame navigation="Меню" status="Готов"><div data-page-path="/setups" hidden>Список</div><div data-page-path="/">Обзор</div></AppFrame>);
  expect(screen.getByRole("main").matches(selector)).toBe(false);
  expect(screen.getByRole("main")).toHaveClass("max-w-7xl");
});

it("captures application-load time once and resolves interface settings before marking it ready", async () => {
  let finishSettings: ((response: Response) => void) | undefined;
  vi.spyOn(Date, "now").mockReturnValue(500_000);
  vi.stubGlobal("fetch", vi.fn((path: string) => path === "/v1/settings/ui"
    ? new Promise<Response>((resolve) => { finishSettings = resolve; })
    : Promise.resolve(new Response(JSON.stringify({ platform: "ready", marketData: "ready" })))));
  function Probe() {
    const load = useApplicationLoad();
    return <output data-testid="application-load">{load?.loadedAtUnix}:{String(load?.interfaceReady)}</output>;
  }
  render(<MemoryRouter initialEntries={["/"]}><Routes><Route element={<AppLayout />}><Route index element={<Probe />} /><Route path="setups" element={<Probe />} /></Route></Routes></MemoryRouter>);
  expect(screen.getByTestId("application-load")).toHaveTextContent("500:false");
  finishSettings?.(new Response(JSON.stringify({ value: {} })));
  await waitFor(() => expect(screen.getByTestId("application-load")).toHaveTextContent("500:true"));
  vi.mocked(Date.now).mockReturnValue(900_000);
  fireEvent.click(screen.getByRole("link", { name: "Сетапы" }));
  expect(screen.getByTestId("application-load")).toHaveTextContent("500:true");
});
