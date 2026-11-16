import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { Separator } from "./separator";

afterEach(cleanup);

describe("Separator", () => {
  it("renders a horizontal separator by default", () => {
    render(<Separator data-testid="separator" />);
    expect(screen.getByTestId("separator")).toHaveClass("h-px", "my-4", "w-full");
  });

  it("renders a vertical separator", () => {
    render(<Separator data-testid="separator" orientation="vertical" />);
    expect(screen.getByTestId("separator")).toHaveClass("w-px");
    expect(screen.getByTestId("separator")).not.toHaveClass("my-4");
  });
});
