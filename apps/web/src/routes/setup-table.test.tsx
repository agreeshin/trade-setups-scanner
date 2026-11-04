import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, it } from "vitest";
import { SetupTable } from "./setup-table";
import type { SetupRow } from "./platform-controls";

afterEach(cleanup);
const setup: SetupRow = { exchange: "bybit", category: "linear", symbol: "BTCUSDT", algorithmId: "price_volume_spike", openTimeUnix: 100, closeTimeUnix: 200, closePrice: "100" };

it("keeps metrics collapsed until requested and shows rounded display ratios and snapshot", async () => {
  render(<SetupTable setups={[{ ...setup, algorithmVersion: "v2", params: { windowMinutes: 5 }, details: { growthPct: "12.345", volumeLongRatio: "3.123456", volumeShortRatio: "4", overLong: true, overShort: true, missingMinutes: 2 } }]} />);
  expect(screen.getByText("3.1235×")).not.toBeVisible();
  await userEvent.click(screen.getByText("Подробности BTCUSDT"));
  expect(screen.getByText("3.1235×")).toBeVisible();
  expect(screen.getByText("4.0000×")).toBeVisible();
  expect(screen.getByText("12.345%")).toBeVisible();
  expect(screen.getByText("Обе базы")).toBeVisible();
  expect(screen.getByText("Пропущено минут").nextElementSibling).toHaveTextContent("2");
  expect(screen.getByText("v2")).toBeVisible();
  expect(screen.getByText("windowMinutes")).toBeVisible();
});

it.each([[true, false, "Длинная база"], [false, true, "Короткая база"], [false, false, "Ни одна"]] as const)("shows branch flags %s/%s without inventing a denominator", async (overLong, overShort, label) => {
  render(<SetupTable setups={[{ ...setup, details: { growthPct: null, volumeLongRatio: null, volumeShortRatio: "Infinity", overLong, overShort, missingMinutes: 0 } }]} />);
  await userEvent.click(screen.getByText("Подробности BTCUSDT"));
  expect(screen.getAllByText("нет базы/не определено")).toHaveLength(2);
  expect(screen.getByText(label)).toBeVisible();
  expect(screen.queryByText(/Infinity/)).not.toBeInTheDocument();
});

it("renders old persisted setups without fabricated metrics", async () => {
  render(<SetupTable setups={[setup]} />);
  await userEvent.click(screen.getByText("Подробности BTCUSDT"));
  expect(screen.getByText("Не сохранены для этой записи.")).toBeVisible();
  expect(screen.queryByText("Версия алгоритма")).not.toBeInTheDocument();
  expect(screen.queryByText("Параметры алгоритма")).not.toBeInTheDocument();
});
