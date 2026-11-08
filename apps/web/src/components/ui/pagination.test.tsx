import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { Pagination } from "./pagination";

afterEach(cleanup);

describe("Pagination", () => {
  it("shows the page summary and disables edge buttons", () => {
    const onPrev = vi.fn();
    const onNext = vi.fn();
    render(<Pagination page={1} pages={5} onPrev={onPrev} onNext={onNext} />);
    expect(screen.getByText("Страница 1 из 5")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Назад" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Далее" })).toBeEnabled();
  });

  it("invokes navigation handlers and accepts a custom summary", () => {
    const onPrev = vi.fn();
    const onNext = vi.fn();
    render(<Pagination page={2} pages={5} onPrev={onPrev} onNext={onNext} summary="Найдено: 40" />);
    fireEvent.click(screen.getByRole("button", { name: "Назад" }));
    expect(onPrev).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "Далее" }));
    expect(onNext).toHaveBeenCalledTimes(1);
    expect(screen.getByText("Найдено: 40")).toBeInTheDocument();
  });
});
