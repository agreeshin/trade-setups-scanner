import { createRef, useState, type ChangeEvent } from "react";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, it, vi } from "vitest";
import { Select } from "./select";

afterEach(cleanup);

it("keeps a real labelled select and native change contract behind the styled dropdown", async () => {
  const change = vi.fn();
  const ref = createRef<HTMLSelectElement>();
  const view = render(<Select ref={ref} aria-label="Направление" defaultValue="short" onChange={change}><option value="short">SHORT</option><option value="long">LONG</option></Select>);
  const select = screen.getByRole("combobox", { name: "Направление" });
  const trigger = view.container.querySelector<HTMLButtonElement>("[data-slot=select-trigger]")!;
  expect(select).toHaveClass("pointer-events-none", "absolute", "opacity-0");
  expect(ref.current).toBe(select);
  expect(trigger).toHaveTextContent("SHORT");
  expect(trigger).toHaveClass("rounded-lg", "bg-background");

  await userEvent.selectOptions(select, "long");
  expect(select).toHaveValue("long");
  expect(trigger).toHaveTextContent("LONG");
  expect(change).toHaveBeenCalledTimes(1);
  expect((change.mock.calls[0]![0] as ChangeEvent<HTMLSelectElement>).target).toBe(select);

  await userEvent.click(trigger);
  await userEvent.click(within(screen.getByRole("dialog")).getByRole("option", { name: "SHORT" }));
  expect(select).toHaveValue("short");
  expect(change).toHaveBeenCalledTimes(2);
  expect((change.mock.calls[1]![0] as ChangeEvent<HTMLSelectElement>).target).toBe(select);
});

it("filters labels without committing and resets search after closing", async () => {
  const change = vi.fn();
  const view = render(<Select aria-label="Набор" searchable searchPlaceholder="Найти набор" emptyText="Нет совпадений" defaultValue="one" onChange={change}>
    <option value="one">Первый запуск</option>
    <option value="two">Второй набор</option>
    <option value="three">Третий набор</option>
  </Select>);
  const trigger = view.container.querySelector<HTMLButtonElement>("[data-slot=select-trigger]")!;
  await userEvent.click(trigger);
  const search = screen.getByPlaceholderText("Найти набор");
  await userEvent.type(search, "ВТОР");
  expect(change).not.toHaveBeenCalled();
  const popup = within(screen.getByRole("dialog"));
  expect(popup.getByRole("option", { name: "Второй набор" })).toBeInTheDocument();
  expect(popup.queryByRole("option", { name: "Первый запуск" })).not.toBeInTheDocument();
  await userEvent.clear(search);
  await userEvent.type(search, "несуществующий");
  expect(screen.getByText("Нет совпадений")).toBeInTheDocument();
  fireEvent.keyDown(search, { key: "Escape" });
  expect(screen.queryByPlaceholderText("Найти набор")).not.toBeInTheDocument();
  await userEvent.click(trigger);
  expect(screen.getByPlaceholderText("Найти набор")).toHaveValue("");
});

it("supports keyboard selection in the searchable list and disabled options", async () => {
  const change = vi.fn();
  const view = render(<Select aria-label="Набор" searchable defaultValue="one" onChange={change}>
    <option value="one">Первый</option>
    <option value="blocked" disabled>Недоступный</option>
    <option value="two">Второй</option>
  </Select>);
  await userEvent.click(view.container.querySelector<HTMLButtonElement>("[data-slot=select-trigger]")!);
  const search = screen.getByPlaceholderText("Найти…");
  fireEvent.keyDown(search, { key: "ArrowDown" });
  fireEvent.keyDown(search, { key: "Enter" });
  expect(screen.getByRole("combobox", { name: "Набор" })).toHaveValue("two");
  expect(change).toHaveBeenCalledTimes(1);
});

it("preserves controlled value, disabled state, form reset, and native validation attributes", async () => {
  function Controlled() {
    const [value, setValue] = useState("short");
    return <form aria-label="Форма"><Select name="direction" required aria-label="Направление" value={value} onChange={(event) => setValue(event.target.value)}>
      <option value="short">SHORT</option><option value="long">LONG</option>
    </Select><button type="reset">Сбросить</button></form>;
  }
  const view = render(<Controlled />);
  const select = screen.getByRole("combobox", { name: "Направление" });
  expect(select).toHaveAttribute("name", "direction");
  expect(select).toBeRequired();
  await userEvent.selectOptions(select, "long");
  expect(view.container.querySelector("[data-slot=select-trigger]")).toHaveTextContent("LONG");
  await userEvent.click(screen.getByRole("button", { name: "Сбросить" }));
  expect(select).toHaveValue("long");

  view.rerender(<Select aria-label="Направление" disabled><option>SHORT</option></Select>);
  expect(screen.getByRole("combobox", { name: "Направление" })).toBeDisabled();
  expect(view.container.querySelector("[data-slot=select-trigger]")).toBeDisabled();
});

it("restores an uncontrolled default value after native form reset", async () => {
  const view = render(<form><Select aria-label="Направление" name="direction" defaultValue="short">
    <option value="short">SHORT</option><option value="long">LONG</option>
  </Select><button type="reset">Сбросить</button></form>);
  const select = screen.getByRole("combobox", { name: "Направление" });
  await userEvent.selectOptions(select, "long");
  expect(view.container.querySelector("[data-slot=select-trigger]")).toHaveTextContent("LONG");
  await userEvent.click(screen.getByRole("button", { name: "Сбросить" }));
  expect(select).toHaveValue("short");
  expect(view.container.querySelector("[data-slot=select-trigger]")).toHaveTextContent("SHORT");
});

it("keeps multiple selects native instead of inventing unsupported custom semantics", () => {
  render(<Select aria-label="Несколько" multiple defaultValue={["one"]}><option value="one">Один</option><option value="two">Два</option></Select>);
  const listbox = screen.getByRole("listbox", { name: "Несколько" });
  expect(listbox).toHaveClass("appearance-none");
  expect(within(listbox).getByRole<HTMLOptionElement>("option", { name: "Один" }).selected).toBe(true);
});

it("opens from the backing keyboard control and restores focus on Escape without changing selection", async () => {
  const change = vi.fn();
  const view = render(<Select aria-label="Набор" searchable value="one" onChange={change}>
    <option value="one">Первый</option><option value="two">Второй</option>
  </Select>);
  const select = screen.getByRole("combobox", { name: "Набор" });
  select.focus();
  await userEvent.keyboard("{ArrowDown}");
  expect(screen.getByRole("listbox")).toBeInTheDocument();
  await userEvent.keyboard("{Escape}");
  expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
  expect(select).toHaveFocus();
  expect(change).not.toHaveBeenCalled();
  view.rerender(<Select aria-label="Набор" searchable value="two" onChange={change}>
    <option value="one">Первый</option><option value="two">Второй</option>
  </Select>);
  expect(view.container.querySelector("[data-slot=select-trigger]")).toHaveTextContent("Второй");
  expect(change).not.toHaveBeenCalled();
  view.rerender(<fieldset disabled><Select aria-label="Набор" searchable value="two" onChange={change}>
    <option value="one">Первый</option><option value="two">Второй</option>
  </Select></fieldset>);
  const disabledTrigger = view.container.querySelector<HTMLButtonElement>("[data-slot=select-trigger]")!;
  expect(disabledTrigger).toBeDisabled();
  await userEvent.click(disabledTrigger);
  expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
  expect(change).not.toHaveBeenCalled();
});
