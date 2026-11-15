import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { AppFrame } from "./app-frame";
import { PageSidebar } from "./page-sidebar";

afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("retains the separate rounded central card and edge navigation", () => {
  const { container } = render(<AppFrame navigation="Меню" status="Готов"><PageSidebar label="Разделы" items={[{ id: "one", label: "Первый" }]} value="one" onValueChange={vi.fn()}>Центральная зона</PageSidebar></AppFrame>);
  expect(container.querySelector('[data-slot="page-card"]')).toHaveClass("rounded-xl", "bg-card");
  expect(container.querySelector('[data-slot="page-sidebar"]')).toHaveClass("rounded-none", "md:right-0", "md:fixed");
  expect(screen.getByRole("main")).toHaveAttribute("data-slot", "app-main");
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
