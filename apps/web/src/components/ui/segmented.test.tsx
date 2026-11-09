import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { SegmentedList, SegmentedTrigger } from "./segmented";

describe("Segmented", () => {
  it("renders list with triggers and forwards tab semantics", () => {
    render(
      <SegmentedList role="tablist" aria-label="Разделы">
        <SegmentedTrigger role="tab" aria-selected>Первый</SegmentedTrigger>
        <SegmentedTrigger role="tab" aria-selected={false} disabled>Второй</SegmentedTrigger>
      </SegmentedList>,
    );
    expect(screen.getByRole("tablist", { name: "Разделы" })).toHaveAttribute("data-slot", "segmented-list");
    expect(screen.getByRole("tab", { name: "Первый" })).toHaveAttribute("aria-selected", "true");
    expect(screen.getByRole("tab", { name: "Первый" })).toHaveClass("aria-selected:bg-primary", "aria-selected:text-primary-foreground");
    const second = screen.getByRole("tab", { name: "Второй" });
    expect(second).toHaveAttribute("data-slot", "segmented-trigger");
    expect(second).toBeDisabled();
  });
});
