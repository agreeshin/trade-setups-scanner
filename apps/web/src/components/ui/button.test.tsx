import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { Button } from "./button";
describe("Button", () => {
  it("renders its children and handles clicks", () => {
    const handleClick = vi.fn();
    render(<Button onClick={handleClick}>Загрузить</Button>);
    const button = screen.getByRole("button", { name: "Загрузить" });
    expect(button).toBeInTheDocument();
    fireEvent.click(button);
    expect(handleClick).toHaveBeenCalledTimes(1);
  });
  it("applies outline variant classes", () => {
    render(<Button variant="outline">Контурная</Button>);
    const button = screen.getByRole("button", { name: "Контурная" });
    expect(button.className).toContain("border");
    expect(button.className).toContain("bg-background");
  });
  it("is disabled with disabled attribute", () => {
    render(<Button disabled>Недоступна</Button>);
    expect(screen.getByRole("button", { name: "Недоступна" })).toBeDisabled();
  });
});
