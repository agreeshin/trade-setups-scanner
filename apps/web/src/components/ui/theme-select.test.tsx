import { afterEach, describe, expect, it } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { ThemeSelect } from "./theme-select";

afterEach(() => {
  cleanup();
  localStorage.clear();
  delete document.documentElement.dataset.theme;
});

describe("ThemeSelect", () => {
  it("persists explicit choices through the single html attribute", () => {
    document.documentElement.dataset.theme = "dark";
    render(<ThemeSelect />);
    const select = screen.getByRole("combobox", { name: "Тема интерфейса" });
    expect(select).toHaveValue("dark");
    fireEvent.change(select, { target: { value: "light" } });
    expect(document.documentElement.dataset.theme).toBe("light");
    expect(localStorage.getItem("trade-setups-theme")).toBe("light");
    fireEvent.change(select, { target: { value: "system" } });
    expect(document.documentElement.dataset.theme).toBe("system");
    expect(localStorage.getItem("trade-setups-theme")).toBe("system");
  });

  it("syncs other tabs and releases its listener when unmounted", () => {
    const { unmount } = render(<ThemeSelect />);
    act(() => { window.dispatchEvent(new StorageEvent("storage", { key: "trade-setups-theme", newValue: "light" })); });
    expect(screen.getByRole("combobox")).toHaveValue("light");
    expect(document.documentElement.dataset.theme).toBe("light");
    act(() => { window.dispatchEvent(new StorageEvent("storage", { key: "unrelated", newValue: "dark" })); });
    expect(screen.getByRole("combobox")).toHaveValue("light");
    act(() => { window.dispatchEvent(new StorageEvent("storage", { key: "trade-setups-theme", newValue: "invalid" })); });
    expect(screen.getByRole("combobox")).toHaveValue("system");
    unmount();
    window.dispatchEvent(new StorageEvent("storage", { key: "trade-setups-theme", newValue: "dark" }));
    expect(document.documentElement.dataset.theme).toBe("system");
  });
});
