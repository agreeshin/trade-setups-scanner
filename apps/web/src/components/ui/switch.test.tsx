import { describe, expect, it } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { Switch } from "./switch";

describe("Switch", () => {
  it("toggles checked state on click", () => {
    render(<Switch aria-label="Live-сканирование" />);
    const control = screen.getByRole("switch", { name: "Live-сканирование" });
    expect(control).not.toBeChecked();
    fireEvent.click(control);
    expect(control).toBeChecked();
  });
});
