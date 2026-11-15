import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { PageHeader, SectionHeader } from "./page-header";

describe("PageHeader", () => {
  it("renders an h1 with a popup description and actions", async () => {
    render(
      <PageHeader
        id="overview-heading"
        title="Обзор"
        description="Состояние сервисов"
        actions={<button type="button">Обновить</button>}
      />,
    );
    expect(screen.getByRole("heading", { level: 1, name: "Обзор" })).toHaveAttribute("id", "overview-heading");
    expect(screen.queryByText("Состояние сервисов")).not.toBeInTheDocument();
    await userEvent.hover(screen.getByRole("button", { name: "О странице" }));
    expect(screen.getByRole("tooltip")).toHaveTextContent("Состояние сервисов");
    expect(screen.getByRole("button", { name: "Обновить" })).toBeInTheDocument();
  });

  it("renders section headers as h2", () => {
    render(<SectionHeader id="live-heading" title="Live-сканирование" />);
    expect(screen.getByRole("heading", { level: 2, name: "Live-сканирование" })).toHaveAttribute("id", "live-heading");
  });
});
