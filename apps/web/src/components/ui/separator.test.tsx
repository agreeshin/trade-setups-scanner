import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { Separator } from "./separator";

afterEach(cleanup);

describe("Separator", () => {
  it("renders a horizontal separator by default", () => {
    render(<Separator data-testid="separator" />);
    expect(screen.getByTestId("separator").className).toContain("h-px");
  });

  it("renders a vertical separator", () => {
    render(<Separator data-testid="separator" orientation="vertical" />);
    expect(screen.getByTestId("separator").className).toContain("w-px");
  });
});
