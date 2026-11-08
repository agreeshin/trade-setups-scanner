import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { EmptyState } from "./empty-state";

describe("EmptyState", () => {
  it("renders title, description and actions", () => {
    render(
      <EmptyState
        title="Сетапы не найдены"
        description="Измените фильтр"
        actions={<button type="button">Сбросить</button>}
      />,
    );
    expect(screen.getByText("Сетапы не найдены")).toBeInTheDocument();
    expect(screen.getByText("Измените фильтр").className).toContain("text-muted-foreground");
    expect(screen.getByRole("button", { name: "Сбросить" })).toBeInTheDocument();
  });
});
