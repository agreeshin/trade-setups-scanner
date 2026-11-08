import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { Checkbox } from "./checkbox";

describe("Checkbox", () => {
  it("toggles checked state on click", () => {
    render(<Checkbox aria-label="Подтверждаю очистку" />);
    const checkbox = screen.getByRole("checkbox", { name: "Подтверждаю очистку" });
    expect(checkbox).not.toBeChecked();
    fireEvent.click(checkbox);
    expect(checkbox).toBeChecked();
  });

  it("supports controlled usage", () => {
    render(<Checkbox aria-label="Включить сбор" checked onCheckedChange={vi.fn()} />);
    expect(screen.getByRole("checkbox", { name: "Включить сбор" })).toBeChecked();
  });
});
