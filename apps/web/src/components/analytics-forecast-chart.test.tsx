import { afterEach, expect, it, vi } from "vitest";
import { act, cleanup, render, screen, within } from "@testing-library/react";
import { AnalyticsForecastChart } from "./analytics-forecast-chart";
import { applyInterfaceSettings, DEFAULT_INTERFACE_SETTINGS } from "../lib/datetime-format";
import type { DailyExpectancyPoint } from "../lib/daily-expectancy";
import type { ForecastCurveRequest } from "../lib/forecast-api";
import { forecastFixture } from "../../test/forecast-fixture";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  applyInterfaceSettings(DEFAULT_INTERFACE_SETTINGS);
});

const fromUnix = Date.parse("2026-01-01T00:00:00Z") / 1000;
const cutoff = Date.parse("2026-01-03T00:30:00Z") / 1000;
const days: DailyExpectancyPoint[] = [0, 1].map((offset) => ({
  day: `2026-01-0${offset + 1}`, dayStartUnix: fromUnix + offset * 86400,
  riskExpectancy: offset === 0 ? 1 : -1, closedCount: 1, tpCount: offset === 0 ? 1 : 0,
  slCount: offset === 0 ? 0 : 1, computedCount: 1, cachedCount: 1,
}));
const request: ForecastCurveRequest = {
  forecast: { method: "decayed_probability", version: 1, halfLifeDays: 14, kappa: 10, priorPolicy: "gross_break_even" },
  trendMode: "blend", trendBlendLambda: 0.7, q: 1, fromUnix, toUnix: fromUnix + 2 * 86400,
  observationCutoff: cutoff, atUnix: cutoff + 86400,
  daily: days.map((day) => ({ dayStartUnix: day.dayStartUnix, tpCount: day.tpCount, slCount: day.slCount })),
};

function mockBrowserTimezone() {
  const DateTimeFormat = Intl.DateTimeFormat;
  vi.spyOn(Intl, "DateTimeFormat").mockImplementation(function (locales, options) { return new DateTimeFormat(locales, { ...options, timeZone: options?.timeZone ?? "Asia/Tokyo" }); });
  expect(Intl.DateTimeFormat().resolvedOptions().timeZone).toBe("Asia/Tokyo");
}

it("uses the configured calendar across midnight and updates mounted labels without shifting or refetching the model", async () => {
  mockBrowserTimezone();
  applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "America/New_York" });
  const response = forecastFixture(request);
  // Fine model samples are deliberately not UTC or local midnights.
  response.curve = response.curve.map((point, index) => ({ ...point, atUnix: [fromUnix + 12345, fromUnix + 90123, cutoff][index]! }));
  response.blendCurve = response.blendCurve!.map((point, index) => ({ ...point, atUnix: response.curve[index]!.atUnix }));
  const fetch = vi.fn<typeof globalThis.fetch>(() => Promise.resolve(new Response(JSON.stringify(response))));
  vi.stubGlobal("fetch", fetch);
  render(<AnalyticsForecastChart request={request} days={days} />);
  const curve = await screen.findByTestId("forecast-model-curve");
  const chart = screen.getByRole("img", { name: "Дневное матожидание и вероятностная модель" });
  const circles = [...chart.querySelectorAll("circle")];
  expect(screen.getByText(/Cutoff C:/)).toHaveTextContent("Cutoff C: 02.01.2026, 19:30:00 (America/New_York)");
  expect(chart).toHaveTextContent("Дата исходного сигнала (America/New_York)");
  expect(chart.querySelector("desc")).toHaveTextContent("календарными подписями America/New_York без пересчёта агрегатов");
  expect(circles.map((point) => point.querySelector("title")?.textContent)).toEqual([
    "31.12.2025 (America/New_York): 1 R, TP 1, SL 0",
    "01.01.2026 (America/New_York): -1 R, TP 0, SL 1",
  ]);
  expect([...chart.querySelectorAll('text[y="273"]')].map((label) => label.textContent)).toEqual(["31.12.2025", "01.01.2026"]);
  expect([...chart.querySelectorAll("title")].some((title) => title.textContent?.includes(" UTC:"))).toBe(false);
  const posted = JSON.parse(fetch.mock.calls[0]![1]!.body as string) as ForecastCurveRequest;
  expect(posted).toEqual(request);
  const expectedX = (atUnix: number) => 66 + (atUnix - fromUnix) / (request.atUnix! - fromUnix) * 666;
  expect(curve.getAttribute("points")!.split(" ").map((point) => Number(point.split(",")[0]))).toEqual(response.curve.map((point) => expectedX(point.atUnix)));
  expect(circles.map((point) => Number(point.getAttribute("cx")))).toEqual(days.map((day) => expectedX(day.dayStartUnix)));
  const geometry = () => [curve.getAttribute("points"), screen.getByTestId("forecast-blend-curve").getAttribute("points"),
    screen.getByTestId("forecast-cutoff").getAttribute("x1"), screen.getByTestId("forecast-flat-future").getAttribute("x1"),
    screen.getByTestId("forecast-flat-future").getAttribute("x2"), ...circles.map((point) => point.getAttribute("cx"))];
  const before = geometry();
  act(() => { applyInterfaceSettings({ timezone: "Pacific/Auckland", dateOrder: "mdy", dateSeparator: "/", yearDigits: 2, time24: false }); });
  expect(screen.getByRole("img", { name: "Дневное матожидание и вероятностная модель" })).toBe(chart);
  expect(screen.getByText(/Cutoff C:/)).toHaveTextContent("Cutoff C: 01/03/26, 01:30:00 PM (Pacific/Auckland)");
  expect(chart).toHaveTextContent("Дата исходного сигнала (Pacific/Auckland)");
  expect(circles[0]?.querySelector("title")).toHaveTextContent("01/01/26 (Pacific/Auckland): 1 R, TP 1, SL 0");
  expect(geometry()).toEqual(before);
  expect(fetch).toHaveBeenCalledTimes(1);
});

it("resolves auto to the browser timezone for calendar labels", async () => {
  mockBrowserTimezone();
  applyInterfaceSettings(DEFAULT_INTERFACE_SETTINGS);
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify(forecastFixture(request))))));
  render(<AnalyticsForecastChart request={request} days={days} />);
  await screen.findByTestId("forecast-model-curve");
  const chart = screen.getByRole("img", { name: "Дневное матожидание и вероятностная модель" });
  expect(chart).toHaveTextContent("Дата исходного сигнала (Asia/Tokyo)");
  expect(screen.getByText(/Cutoff C:/)).toHaveTextContent("Cutoff C: 03.01.2026, 09:30:00 (Asia/Tokyo)");
  expect(within(chart).getByText("01.01.2026", { selector: "text" })).toBeInTheDocument();
});
