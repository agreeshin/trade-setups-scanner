import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { Spinner } from "./spinner";

describe("Spinner", () => {
  it("exposes an accessible status with the given label", () => {
    render(<Spinner label="Загрузка данных" />);
    expect(screen.getByRole("status")).toHaveAttribute("aria-label", "Загрузка данных");
  });
});
