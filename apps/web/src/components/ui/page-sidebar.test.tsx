import { useState } from "react";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, it, vi } from "vitest";
import { PageSidebar, PageSidebarPanel } from "./page-sidebar";

afterEach(cleanup);

function Harness() {
  const [selected, setSelected] = useState("first");
  return <PageSidebar label="Подразделы" items={[{ id: "first", label: "Первый", status: "Live-сканирование включено" }, { id: "second", label: "Второй" }]} value={selected} onValueChange={setSelected}>
    <PageSidebarPanel value="first"><input aria-label="Черновик" /></PageSidebarPanel>
    <PageSidebarPanel value="second"><p>Вторая панель</p></PageSidebarPanel>
  </PageSidebar>;
}

it("links a mobile-first/right-desktop sidebar to retained, accessible panels", async () => {
  const { container } = render(<Harness />);
  const first = screen.getByRole("tab", { name: "Первый" });
  const draft = screen.getByRole("textbox", { name: "Черновик" });
  fireEvent.change(draft, { target: { value: "Сохранённый черновик" } });
  expect(first).toHaveAccessibleDescription("Live-сканирование включено");
  expect(first).toHaveAttribute("type", "button");
  expect(document.getElementById(first.getAttribute("aria-controls")!)).toBe(screen.getByRole("tabpanel", { name: "Первый" }));
  const layout = container.querySelector('[data-slot="page-sidebar-layout"]')!;
  expect(layout.firstElementChild).toHaveAttribute("data-slot", "page-sidebar");
  expect(layout.firstElementChild).toHaveClass("md:col-start-2");
  await userEvent.click(screen.getByRole("tab", { name: "Второй" }));
  expect(screen.getAllByRole("tabpanel")).toHaveLength(1);
  expect(draft).not.toBeVisible();
  expect(draft).toBeInTheDocument();
  await userEvent.keyboard("{Home}");
  expect(first).toHaveFocus();
  expect(first).toHaveAttribute("aria-selected", "true");
  expect(draft).toBeVisible();
  expect(draft).toHaveValue("Сохранённый черновик");
  await userEvent.keyboard("{ArrowDown}");
  expect(screen.getByRole("tab", { name: "Второй" })).toHaveFocus();
  await userEvent.keyboard("{ArrowDown}");
  expect(first).toHaveFocus();
});

it("does not add empty navigation for a page with no subdivisions", () => {
  render(<PageSidebar label="Пусто" items={[]} value="" onValueChange={vi.fn()}><p>Содержимое</p></PageSidebar>);
  expect(screen.queryByRole("tablist")).not.toBeInTheDocument();
  expect(screen.getByText("Содержимое")).toBeVisible();
});

it("retains every accepted controlled selection without requiring a sidebar click", () => {
  const change = vi.fn();
  const layout = (value: string) => <PageSidebar label="Управляемые панели" items={[{ id: "first", label: "Первый" }, { id: "second", label: "Второй" }]} value={value} onValueChange={change}>
    <PageSidebarPanel value="first" lazy>Первая панель</PageSidebarPanel>
    <PageSidebarPanel value="second" lazy><input aria-label="Внешний черновик" /></PageSidebarPanel>
  </PageSidebar>;
  const view = render(layout("first"));
  expect(screen.queryByLabelText("Внешний черновик")).not.toBeInTheDocument();
  view.rerender(layout("second"));
  const draft = screen.getByLabelText("Внешний черновик");
  fireEvent.change(draft, { target: { value: "Извне" } });
  view.rerender(layout("first"));
  expect(draft).toBeInTheDocument(); expect(draft).not.toBeVisible();
  view.rerender(layout("second"));
  expect(screen.getByLabelText("Внешний черновик")).toBe(draft);
  expect(draft).toHaveValue("Извне");
});

it("does not mount a lazy panel when its controlled selection request is declined", async () => {
  const change = vi.fn();
  render(<PageSidebar label="Управляемые панели" items={[{ id: "first", label: "Первый" }, { id: "second", label: "Второй" }]} value="first" onValueChange={change}>
    <PageSidebarPanel value="first" lazy>Первая панель</PageSidebarPanel>
    <PageSidebarPanel value="second" lazy><input aria-label="Отклонённый черновик" /></PageSidebarPanel>
  </PageSidebar>);
  await userEvent.click(screen.getByRole("tab", { name: "Второй" }));
  expect(change).toHaveBeenCalledWith("second");
  expect(screen.getByRole("tab", { name: "Первый" })).toHaveAttribute("aria-selected", "true");
  expect(screen.queryByLabelText("Отклонённый черновик")).not.toBeInTheDocument();
});
