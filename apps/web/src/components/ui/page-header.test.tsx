import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { PageHeader, SectionHeader } from "./page-header";

describe("PageHeader", () => {
  it("renders an h1 with description and actions", () => {
    render(
      <PageHeader
        id="overview-heading"
        title="Обзор"
        description="Состояние сервисов"
        actions={<button type="button">Обновить</button>}
      />,
    );
    expect(screen.getByRole("heading", { level: 1, name: "Обзор" })).toHaveAttribute("id", "overview-heading");
    expect(screen.getByText("Состояние сервисов").className).toContain("text-muted-foreground");
    expect(screen.getByRole("button", { name: "Обновить" })).toBeInTheDocument();
  });

  it("renders section headers as h2", () => {
    render(<SectionHeader id="live-heading" title="Live-сканирование" />);
    expect(screen.getByRole("heading", { level: 2, name: "Live-сканирование" })).toHaveAttribute("id", "live-heading");
  });
});
