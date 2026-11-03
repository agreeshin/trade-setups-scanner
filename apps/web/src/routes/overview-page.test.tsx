import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { Provider } from "react-redux";
import { AppRoutes } from "../app-routes";
import { createStore } from "../store";
function renderAppAt(initialPath: string) {
  return render(
    <Provider store={createStore()}>
      <MemoryRouter initialEntries={[initialPath]}>
        <AppRoutes />
      </MemoryRouter>
    </Provider>,
  );
}
describe("application routing shell", () => {
  it("shows overview heading and navigation on the index route", () => {
    renderAppAt("/");
    expect(screen.getByRole("heading", { name: "Обзор" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Настройки" })).toBeInTheDocument();
  });
  it("shows settings page on /settings", () => {
    renderAppAt("/settings");
    expect(
      screen.getByRole("heading", { name: "Настройки" }),
    ).toBeInTheDocument();
  });
});
