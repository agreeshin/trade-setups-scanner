import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, it, vi } from "vitest";
import { Select } from "./select";

afterEach(cleanup);
it("styles the shared select without changing native selection and disabled semantics", async () => {
  const change = vi.fn();
  const view = render(<Select aria-label="Направление" defaultValue="short" onChange={change}><option value="short">SHORT</option><option value="long">LONG</option></Select>);
  const select = screen.getByRole("combobox");
  expect(select).toHaveClass("appearance-none", "pr-9");
  expect(select.parentElement?.querySelector("svg")).toHaveClass("right-3", "pointer-events-none");
  await userEvent.selectOptions(select, "long");
  expect(select).toHaveValue("long");
  expect(change).toHaveBeenCalledTimes(1);
  view.rerender(<Select aria-label="Направление" disabled><option>SHORT</option></Select>);
  expect(screen.getByRole("combobox")).toBeDisabled();
});
