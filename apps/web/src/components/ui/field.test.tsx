import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { Field } from "./field";
import { Input } from "./input";

afterEach(cleanup);

describe("Field", () => {
  it("links the label to the control and exposes the hint", () => {
    render(
      <Field label="Инструмент" hint="Например BTCUSDT">
        {(control) => <Input {...control} />}
      </Field>,
    );
    const input = screen.getByRole("textbox");
    expect(screen.getByText("Инструмент")).toHaveAttribute("for", input.id);
    expect(input.getAttribute("aria-describedby")).toContain("hint");
    expect(screen.getByText("Например BTCUSDT")).toBeInTheDocument();
  });

  it("marks invalid controls and announces the error", () => {
    render(
      <Field label="Возраст" error="Укажите целое число">
        {(control) => <Input {...control} />}
      </Field>,
    );
    const input = screen.getByRole("textbox");
    expect(input).toHaveAttribute("aria-invalid", "true");
    expect(input.getAttribute("aria-describedby")).toContain("error");
    expect(screen.getByRole("alert")).toHaveTextContent("Укажите целое число");
  });
});
