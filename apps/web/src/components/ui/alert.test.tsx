import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { Alert, AlertDescription, AlertTitle } from "./alert";

describe("Alert", () => {
  it("renders title and description with the destructive variant and role", () => {
    render(
      <Alert variant="destructive" role="alert">
        <AlertTitle>Платформа недоступна</AlertTitle>
        <AlertDescription>Повторите позже</AlertDescription>
      </Alert>,
    );
    const alert = screen.getByRole("alert");
    expect(alert.className).toContain("border-danger");
    expect(screen.getByText("Платформа недоступна").tagName).toBe("P");
    expect(screen.getByText("Повторите позже").className).toContain("text-muted-foreground");
  });

  it("defaults to the info variant", () => {
    render(
      <Alert role="status">
        <AlertTitle>Проверка</AlertTitle>
      </Alert>,
    );
    expect(screen.getByRole("status").className).toContain("border-info");
  });
});
