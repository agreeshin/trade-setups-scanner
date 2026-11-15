import { useState } from "react";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, it, vi } from "vitest";
import { applyInterfaceSettings, DEFAULT_INTERFACE_SETTINGS, formatCalendarInput } from "../../lib/datetime-format";
import { Input } from "./input";
import { Field } from "./field";

afterEach(() => { cleanup(); applyInterfaceSettings(DEFAULT_INTERFACE_SETTINGS); vi.restoreAllMocks(); });

const formats = (["dmy", "mdy", "ymd"] as const).flatMap((dateOrder) => ([".", "/", "-"] as const).flatMap((dateSeparator) => ([2, 4] as const).map((yearDigits) => ({ ...DEFAULT_INTERFACE_SETTINGS, dateOrder, dateSeparator, yearDigits }))));
it.each(formats)("formats a date with $dateOrder, $dateSeparator and $yearDigits year digits without changing ISO input values", (format) => {
  applyInterfaceSettings(format);
  render(<Field label="Дата">{(control) => <Input {...control} type="date" value="2026-01-09" readOnly />}</Field>);
  const year = format.yearDigits === 2 ? "26" : "2026";
  const parts = { dmy: ["09", "01", year], mdy: ["01", "09", year], ymd: [year, "01", "09"] };
  expect(screen.getByText(parts[format.dateOrder].join(format.dateSeparator))).toBeVisible();
  expect(screen.getByLabelText("Дата")).toHaveValue("2026-01-09");
});

it.each(["Pacific/Auckland", "America/New_York"])("does not move a calendar date or local input time in interface zone %s", (timezone) => {
  applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone });
  render(<Input type="datetime-local" aria-label="Начало" value="2026-01-09T00:05" readOnly />);
  expect(screen.getByText("09.01.2026, 00:05")).toBeVisible();
  expect(screen.getByLabelText("Начало")).toHaveValue("2026-01-09T00:05");
});

it("reacts to settings arriving after mount and disposes its subscription", () => {
  const remove = vi.spyOn(window, "removeEventListener");
  const view = render(<Input type="date" aria-label="Дата" value="2026-01-09" readOnly />);
  expect(screen.getByText("09.01.2026")).toBeVisible();
  act(() => { applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, dateOrder: "ymd", dateSeparator: "/", yearDigits: 2 }); });
  expect(screen.getByText("26/01/09")).toBeVisible();
  view.unmount();
  expect(remove).toHaveBeenCalledWith("interface-datetime-settings-change", expect.any(Function));
});

it("keeps native change/blur events, ISO submissions, constraints, calendar affordance and clearing", () => {
  const changed = vi.fn(); const blurred = vi.fn();
  function Controlled() {
    const [value, setValue] = useState("2026-01-09T14:05");
    return <Input type="datetime-local" aria-label="Начало" min="2026-01-01T00:00" max="2026-12-31T23:59" step={60} required value={value} onBlur={blurred} onChange={(event) => { changed(event.target.value); setValue(event.target.value); }} />;
  }
  const view = render(<Controlled />);
  const input = screen.getByLabelText("Начало");
  expect(input).toHaveAttribute("min", "2026-01-01T00:00"); expect(input).toHaveAttribute("max", "2026-12-31T23:59");
  expect(input).toHaveAttribute("step", "60"); expect(input).toBeRequired();
  expect(view.container.querySelector("svg")).toHaveClass("right-3", "top-1/2", "pointer-events-none");
  fireEvent.change(input, { target: { value: "2026-02-10T01:02" } });
  expect(changed).toHaveBeenLastCalledWith("2026-02-10T01:02");
  expect(screen.getByText("10.02.2026, 01:02")).toBeVisible();
  fireEvent.blur(input); expect(blurred).toHaveBeenCalledOnce();
  fireEvent.change(input, { target: { value: "" } });
  expect(changed).toHaveBeenLastCalledWith("");
  expect(screen.getByText("ДД.ММ.ГГГГ, ЧЧ:ММ")).toBeVisible();
});

it("supports uncontrolled values and opens the native picker from the whole selector", async () => {
  render(<Input type="date" aria-label="Дата" defaultValue="2026-01-09" />);
  const input = screen.getByLabelText("Дата");
  const picker = vi.fn(); Object.defineProperty(input, "showPicker", { value: picker });
  await userEvent.click(input); expect(picker).toHaveBeenCalledOnce();
  fireEvent.change(input, { target: { value: "2026-01-10" } });
  expect(screen.getByText("10.01.2026")).toBeVisible();
});

it("preserves disabled/read-only controls and the 12-hour display including seconds", () => {
  applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, time24: false });
  expect(formatCalendarInput("2026-01-09T14:05:07.123")).toBe("09.01.2026, 02:05:07.123 PM");
  expect(formatCalendarInput("2026-01-09T00:05")).toBe("09.01.2026, 12:05 AM");
  const view = render(<Input type="datetime-local" aria-label="Начало" value="2026-01-09T14:05" disabled />);
  expect(screen.getByLabelText("Начало")).toBeDisabled();
  view.rerender(<Input type="datetime-local" aria-label="Начало" value="2026-01-09T14:05" readOnly />);
  expect(screen.getByLabelText("Начало")).toHaveAttribute("readonly");
  expect(screen.getByText("09.01.2026, 02:05 PM")).toBeVisible();
});
