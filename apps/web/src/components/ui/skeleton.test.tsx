import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { Skeleton } from "./skeleton";

afterEach(cleanup);

describe("Skeleton", () => {
  it("renders a hidden rounded theme gradient with motion-safe shimmer", () => {
    render(<Skeleton data-testid="skeleton" />);
    const skeleton = screen.getByTestId("skeleton");
    expect(skeleton).toHaveAttribute("aria-hidden", "true");
    expect(skeleton).toHaveClass("rounded-md", "bg-linear-to-r", "from-muted", "via-muted-foreground/15", "to-muted", "bg-size-[200%_100%]", "motion-safe:animate-skeleton-shimmer");
    expect(skeleton).not.toHaveClass("animate-pulse", "animate-skeleton-shimmer");
  });

  it("forwards attributes and merges caller dimensions without losing the shared presentation", () => {
    render(<Skeleton data-testid="custom" className="h-6 w-40 rounded-lg" />);
    expect(screen.getByTestId("custom")).toHaveClass("h-6", "w-40", "rounded-lg", "motion-safe:animate-skeleton-shimmer");
    expect(screen.getByTestId("custom")).not.toHaveClass("rounded-md");
  });
});
