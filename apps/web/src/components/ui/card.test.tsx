import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { Card, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from "./card";

afterEach(cleanup);

describe("Card", () => {
  it("renders the full family composition", () => {
    render(
      <Card>
        <CardHeader>
          <CardTitle>Состояние сервисов</CardTitle>
          <CardDescription>Последний ответ API</CardDescription>
        </CardHeader>
        <CardContent>готова</CardContent>
        <CardFooter>обновлено</CardFooter>
      </Card>,
    );
    expect(screen.getByText("Состояние сервисов").tagName).toBe("H3");
    expect(screen.getByText("Последний ответ API")).toBeInTheDocument();
    expect(screen.getByText("готова")).toBeInTheDocument();
    expect(screen.getByText("обновлено")).toBeInTheDocument();
  });

  it("keeps the bordered card surface", () => {
    render(<Card data-testid="card">тело</Card>);
    const card = screen.getByTestId("card");
    expect(card.className).toContain("border");
    expect(card.className).toContain("bg-card");
  });
});
