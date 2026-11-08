import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { Skeleton } from "./skeleton";

describe("Skeleton", () => {
  it("renders a hidden pulsing placeholder", () => {
    render(<Skeleton data-testid="skeleton" />);
    const skeleton = screen.getByTestId("skeleton");
    expect(skeleton.className).toContain("animate-pulse");
    expect(skeleton).toHaveAttribute("aria-hidden", "true");
  });
});
