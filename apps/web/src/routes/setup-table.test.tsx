import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, it, vi } from "vitest";
import { SetupTable } from "./setup-table";
import type { SetupRow } from "./platform-controls";
import { applyInterfaceSettings } from "../lib/datetime-format";
import type { AdaptiveExpectancySnapshot, AdaptiveRecommendation } from "../lib/setup-recommendation";
import { RouteActiveContext } from "./route-active";

afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); applyInterfaceSettings({ timezone: "auto" }); });
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
  expect(screen.getByText("WATCH")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: /Условия сетапа/ })).not.toBeInTheDocument();
});

const signalAt = Date.parse("2026-11-12T19:43:00Z") / 1000;
const recommendation: AdaptiveRecommendation = { direction: "short", tpPct: 14, slPct: 4, rawR: 0.32, meanR: 0.21, robustScore: 0.01, timeShiftMinutes: 3, priceChangePct: 1, externalName: "<b>Сохранённое имя</b>", studyId: 42, generationId: 7, slotId: 5, ownerId: "admin", clusterEnabled: true, clusterGapMinutes: 480,
  params: { priceGrowthPct: "12", windowMinutes: 5, baselineLongDays: 30, baselineShortDays: 7, volumeMultiplier: "3", cooldownMinutes: 5, maxGapPercent: "1", maxConsecutiveGapMinutes: 5, requiredRecentMinutes: 10 },
  provenance: { algorithmId: "price_volume_spike", algorithmVersion: "1.1.1", optimizerVersion: "continuous-v1", seed: 42, snapshotHash: "frozen", capturedAt: "2026-11-12T19:43:00Z", fromUnix: signalAt - 86400, toUnix: signalAt, observationCutoff: signalAt } };

const expectancy: AdaptiveExpectancySnapshot = {
  calculationVersion: 2, timezone: "UTC", studyId: 42, generationId: 7, slotId: 5, derivedSnapshotId: "derived-42", sourceSnapshotHash: "frozen", sourceCutoff: signalAt,
  fromUnix: signalAt - 86400, toUnix: signalAt, periodDays: 1, selectedMeanR: { status: "available", value: 0.32 }, tpCount: 5, slCount: 3, closedCount: 8,
  winRatePct: { status: "available", value: 62.5 }, matrixMeanR: { status: "available", value: 0.21 }, trendDeltaR: { status: "available", value: -0.12 },
  forecastR: { status: "available", value: 0.44 }, forecastAtUnix: signalAt,
  imageKey: { studyId: "42", derivedSnapshotId: "derived-42", inputSnapshotHash: "frozen", tpPct: 14, slPct: 4, calculationVersion: 2, timezone: "UTC", renderVersion: 1 },
};

it("exposes the immutable recommendation on hover/focus and tap dialog without replacing source WATCH", async () => {
  applyInterfaceSettings({ timezone: "UTC" }); vi.spyOn(Date, "now").mockReturnValue(signalAt * 1000);
  const { container } = render(<SetupTable setups={[{ ...setup, closeTimeUnix: signalAt, recommendation }]} />);
  expect(screen.getByText("WATCH")).toBeInTheDocument();
  expect(screen.getByLabelText("Рекомендация SHORT")).toHaveTextContent("SHORT");
  const trigger = screen.getByRole("button", { name: /Условия сетапа BTCUSDT.*исследование 42/ });
  expect(container.querySelector("b")).toBeNull();
  await userEvent.hover(trigger);
  expect(screen.getByRole("tooltip")).toHaveTextContent("выше 101.00 USDT");
  expect(screen.getByRole("tooltip")).toHaveTextContent("Среднее МО за последние 1 суток: +0.32 R");
  expect(screen.getByRole("tooltip")).toHaveTextContent("В среднем для других уровней TP и SL: +0.21 R");
  expect(screen.getByRole("tooltip")).toHaveTextContent("Прогноз МО торговли: недоступно");
  expect(trigger).toHaveAttribute("aria-describedby", screen.getByRole("tooltip").id);
  await userEvent.unhover(trigger);
  expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
  await userEvent.tab();
  expect(trigger).toHaveFocus(); expect(screen.getByRole("tooltip")).toBeInTheDocument();
  await userEvent.keyboard("{Escape}");
  expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
  await userEvent.click(trigger);
  const dialog = screen.getByRole("dialog", { name: "Условия сетапа BTCUSDT" });
  expect(dialog).toHaveTextContent("Цена: 100.00 USDT");
  expect(dialog).toHaveTextContent("<b>Сохранённое имя</b>");
  expect(dialog).toHaveTextContent("поколение #7");
  expect(dialog).not.toHaveTextContent("сетапов в день");
  expect(dialog.querySelector("b")).toBeNull();
  await userEvent.click(screen.getByRole("button", { name: "Закрыть" }));
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(trigger).toHaveFocus();
});

it("marks an elapsed recommendation and keeps two profile events instead of collapsing their shared raw time", async () => {
  vi.spyOn(Date, "now").mockReturnValue((signalAt + 600) * 1000);
  render(<SetupTable setups={[
    { ...setup, id: 1, closeTimeUnix: signalAt, recommendation },
    { ...setup, id: 2, closeTimeUnix: signalAt, recommendation: { ...recommendation, direction: "long", studyId: 43, externalName: "Второй снимок" } },
  ]} />);
  expect(screen.getAllByRole("row")).toHaveLength(3);
  expect(screen.getAllByText("WATCH")).toHaveLength(2);
  await userEvent.click(screen.getByRole("button", { name: /Условия сетапа BTCUSDT.*исследование 43/ }));
  const dialog = screen.getByRole("dialog");
  expect(dialog).toHaveTextContent("рекомендуемое время уже прошло");
  expect(dialog).toHaveTextContent("будет ниже 99.00 USDT");
  expect(dialog).not.toHaveTextContent("через 3 мин");
});
it("retains an open recommendation when refreshed rows change, disappear or become empty", async () => {
  vi.spyOn(Date, "now").mockReturnValue(signalAt * 1000);
  const selected = { ...setup, id: 1, closeTimeUnix: signalAt, recommendation };
  const view = render(<SetupTable setups={[selected]} />);
  await userEvent.click(screen.getByRole("button", { name: /Условия сетапа BTCUSDT.*исследование 42/ }));
  const dialog = screen.getByRole("dialog");
  view.rerender(<SetupTable setups={[{ ...selected, closePrice: "999", recommendation: { ...recommendation, studyId: 99, generationId: 88, externalName: "Новое имя" } }]} />);
  expect(screen.getByRole("dialog")).toBe(dialog);
  expect(dialog).toHaveTextContent("исследование #42"); expect(dialog).toHaveTextContent("поколение #7");
  expect(dialog).toHaveTextContent("Цена: 100.00 USDT"); expect(dialog).not.toHaveTextContent("Новое имя");
  view.rerender(<SetupTable setups={[{ ...selected, id: 2, symbol: "ETHUSDT" }]} />);
  expect(screen.getByRole("dialog")).toBe(dialog); expect(dialog).toHaveTextContent("Условия сетапа BTCUSDT");
  view.rerender(<SetupTable setups={[]} />);
  expect(screen.getByRole("dialog")).toBe(dialog);
  await userEvent.click(screen.getByRole("button", { name: "Закрыть" }));
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(screen.getByRole("region", { name: "Список сетапов" })).toHaveFocus();
});

it("keeps a visible tooltip snapshot stable while its row props refresh", () => {
  vi.spyOn(Date, "now").mockReturnValue(signalAt * 1000);
  const selected = { ...setup, id: 1, closeTimeUnix: signalAt, recommendation };
  const view = render(<SetupTable setups={[selected]} />);
  fireEvent.focus(screen.getByRole("button", { name: /Условия сетапа BTCUSDT/ }));
  expect(screen.getByRole("tooltip")).toHaveTextContent("Цена: 100.00 USDT");
  view.rerender(<SetupTable setups={[{ ...selected, closePrice: "999", recommendation: { ...recommendation, externalName: "Новое имя" } }]} />);
  expect(screen.getByRole("tooltip")).toHaveTextContent("Цена: 100.00 USDT");
  expect(screen.getByRole("tooltip")).not.toHaveTextContent("Новое имя");
});

it("updates only the visible display clock and releases its interval on unmount", () => {
  vi.useFakeTimers(); vi.setSystemTime(signalAt * 1000);
  const view = render(<RouteActiveContext.Provider value><SetupTable setups={[{ ...setup, closeTimeUnix: signalAt, recommendation }]} /></RouteActiveContext.Provider>);
  expect(vi.getTimerCount()).toBe(0);
  fireEvent.focus(screen.getByRole("button", { name: /Условия сетапа BTCUSDT/ }));
  expect(vi.getTimerCount()).toBe(1);
  act(() => { vi.advanceTimersByTime(181_000); });
  expect(screen.getByRole("tooltip")).toHaveTextContent("рекомендуемое время уже прошло");
  view.rerender(<RouteActiveContext.Provider value={false}><SetupTable setups={[{ ...setup, closeTimeUnix: signalAt, recommendation }]} /></RouteActiveContext.Provider>);
  expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
  expect(vi.getTimerCount()).toBe(0);
  view.unmount(); expect(vi.getTimerCount()).toBe(0);
});

it("renders semantic bold/code snapshot text on keyboard and touch without any chart or analytics fetch", async () => {
  applyInterfaceSettings({ timezone: "UTC" }); vi.spyOn(Date, "now").mockReturnValue(signalAt * 1000);
  const fetch = vi.fn(); vi.stubGlobal("fetch", fetch);
  render(<SetupTable setups={[{ ...setup, closeTimeUnix: signalAt, recommendation: { ...recommendation, rawSignalAt: signalAt, expectancy } }]} />);
  const trigger = screen.getByRole("button", { name: /Условия сетапа BTCUSDT.*исследование 42/ });
  await userEvent.tab(); expect(trigger).toHaveFocus();
  const tooltip = screen.getByRole("tooltip");
  expect([...tooltip.querySelectorAll("strong")].map((node) => node.textContent)).toEqual(["Условия сделки:", "Матожидание торговли:"]);
  const codeValues = [...tooltip.querySelectorAll("code")].map((node) => node.textContent);
  expect(codeValues).toEqual(["BTCUSDT", "100.00", "19:46:00 UTC", "3", "14%", "4%", "101.00", "+0.44 R", "1", "+0.32 R", "1", "-0.12 R", "62.5%", "+0.21 R", "42"]);
  expect(tooltip).toHaveTextContent("Исследование №42"); expect(tooltip).not.toHaveTextContent("2026");
  expect(tooltip.querySelector("b")).toBeNull(); expect(tooltip).toHaveTextContent("<b>Сохранённое имя</b>");
  await userEvent.keyboard("{Enter}");
  const dialog = screen.getByRole("dialog", { name: "Условия сетапа BTCUSDT" });
  expect(dialog.querySelector("pre")?.textContent).toBe(tooltip.querySelector("pre")?.textContent);
  expect(dialog.querySelectorAll("strong")).toHaveLength(2); expect(dialog.querySelector("img")).toBeNull();
  await userEvent.keyboard("{Escape}"); expect(screen.queryByRole("dialog")).not.toBeInTheDocument(); expect(trigger).toHaveFocus();
  await userEvent.pointer([{ keys: "[TouchA>]", target: trigger }, { keys: "[/TouchA]" }]);
  expect(screen.getByRole("dialog")).toHaveTextContent("Прогноз МО торговли: +0.44 R");
  expect(fetch).not.toHaveBeenCalled();
});

it("retains frozen enrichment in hover and dialog while incoming rows change or are removed", async () => {
  vi.spyOn(Date, "now").mockReturnValue(signalAt * 1000);
  const selected = { ...setup, id: 1, closeTimeUnix: signalAt, recommendation: { ...recommendation, expectancy } };
  const view = render(<SetupTable setups={[selected]} />);
  const trigger = screen.getByRole("button", { name: /Условия сетапа BTCUSDT.*исследование 42/ });
  fireEvent.focus(trigger);
  expect(screen.getByRole("tooltip")).toHaveTextContent("Прогноз МО торговли: +0.44 R");
  const updated = { ...selected, recommendation: { ...selected.recommendation, expectancy: { ...expectancy, forecastR: { status: "available" as const, value: 0.99 } } } };
  view.rerender(<SetupTable setups={[updated]} />);
  expect(screen.getByRole("tooltip")).toHaveTextContent("Прогноз МО торговли: +0.44 R");
  expect(screen.getByRole("tooltip")).not.toHaveTextContent("+0.99 R");
  await userEvent.click(trigger);
  expect(screen.getByRole("dialog")).toHaveTextContent("Прогноз МО торговли: +0.44 R");
  view.rerender(<SetupTable setups={[]} />);
  expect(screen.getByRole("dialog")).toHaveTextContent("Прогноз МО торговли: +0.44 R");
  expect(screen.getByRole("dialog")).toHaveTextContent("Исследование №42");
});

it("shows malformed enrichment as unavailable but preserves WATCH, direction and saved trade conditions", async () => {
  vi.spyOn(Date, "now").mockReturnValue(signalAt * 1000);
  render(<SetupTable setups={[{ ...setup, closeTimeUnix: signalAt, recommendation: { ...recommendation, expectancy: { ...expectancy, forecastAtUnix: signalAt + 180 } } }]} />);
  expect(screen.getByText("WATCH")).toBeInTheDocument(); expect(screen.getByLabelText("Рекомендация SHORT")).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: /Условия сетапа BTCUSDT.*исследование 42/ }));
  const dialog = screen.getByRole("dialog");
  expect(dialog).toHaveTextContent("Прогноз МО торговли: недоступно (снимок метрик некорректен или не относится к этому сетапу)");
  expect(dialog).toHaveTextContent("выше 101.00 USDT"); expect(dialog).toHaveTextContent("+0.32 R"); expect(dialog).toHaveTextContent("+0.21 R");
  expect(dialog).not.toHaveTextContent("+0.44 R"); expect(dialog).not.toHaveTextContent("62.5%");
});
