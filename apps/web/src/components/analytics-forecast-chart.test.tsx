import { afterEach, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { AnalyticsForecastChart, DailyCalendarGrid } from "./analytics-forecast-chart";
import { applyInterfaceSettings, DEFAULT_INTERFACE_SETTINGS, zonedDayStart } from "../lib/datetime-format";
import type { DailyExpectancyPoint } from "../lib/daily-expectancy";
import type { ForecastCurveRequest, ForecastCurveResponse } from "../lib/forecast-api";
import { forecastFixture, withDisplayCurve } from "../../test/forecast-fixture";

afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals(); applyInterfaceSettings(DEFAULT_INTERFACE_SETTINGS); });

const fromUnix = Date.parse("2025-12-31T00:00:00Z") / 1000;
const cutoff = Date.parse("2026-01-15T00:30:00Z") / 1000;
const days: DailyExpectancyPoint[] = [0, 1, 4].map((offset, index) => ({
  day: new Date((fromUnix + offset * 86400) * 1000).toISOString().slice(0, 10), dayStartUnix: fromUnix + offset * 86400,
  riskExpectancy: index === 1 ? -1 : 1, closedCount: 1, tpCount: index === 1 ? 0 : 1,
  slCount: index === 1 ? 1 : 0, computedCount: 1, cachedCount: 1,
}));
const request: ForecastCurveRequest = {
  forecast: { method: "decayed_probability", version: 1, halfLifeDays: 14, kappa: 10, priorPolicy: "gross_break_even" },
  trendMode: "blend", trendBlendLambda: 0.7, q: 1, fromUnix, toUnix: fromUnix + 10 * 86400,
  observationCutoff: cutoff, atUnix: cutoff + 86400,
  daily: days.map((day) => ({ dayStartUnix: day.dayStartUnix, tpCount: day.tpCount, slCount: day.slCount })),
};
const mock = (response: ForecastCurveResponse) => {
  const fetch = vi.fn<typeof globalThis.fetch>(() => Promise.resolve(new Response(JSON.stringify(response))));
  vi.stubGlobal("fetch", fetch); return fetch;
};
const observedCircles = (chart: HTMLElement) => [...chart.querySelectorAll("circle")].filter((circle) => circle.querySelector("title")?.textContent?.includes("TP "));

it.each(["mean", "forecast", "blend"] as const)("labels the final selected %s scalar on the last signal day, not cutoff/now, without changing scientific inputs", async (mode) => {
  applyInterfaceSettings({ timezone: "UTC" });
  const input = { ...request, trendMode: mode };
  const response = forecastFixture(input);
  const fetch = mock(response);
  render(<AnalyticsForecastChart request={input} days={days} />);
  const label = await screen.findByTestId("forecast-endpoint-label");
  expect(label).toHaveTextContent(`${response.decisionR} R`);
  expect(label).toHaveAttribute("data-canonical-at-unix", String(cutoff));
  expect(label).toHaveAttribute("data-signal-day-unix", String(days.at(-1)!.dayStartUnix));
  expect(label).toHaveAttribute("x", "732");
  expect(label).toHaveAccessibleName(expect.stringContaining(mode === "mean" ? "M:" : mode === "forecast" ? "F:" : "B:"));
  expect(screen.queryByTestId("forecast-flat-future")).not.toBeInTheDocument();
  expect(screen.queryByTestId("blend-flat-future")).not.toBeInTheDocument();
  expect(screen.queryByTestId("forecast-cutoff")).not.toBeInTheDocument();
  const line = screen.getByTestId(mode === "forecast" ? "forecast-model-curve" : "forecast-blend-curve");
  expect(line.getAttribute("points")!.split(" ").at(-1)!.split(",")[0]).toBe("732");
  expect(Number(label.getAttribute("y"))).toBeGreaterThan(0);
  expect(JSON.parse(fetch.mock.calls[0]![1]!.body as string)).toEqual(input);
  expect(input.observationCutoff).toBe(cutoff);
  expect(response.curve.at(-1)!.atUnix).toBe(cutoff);
  expect(screen.getByLabelText("Чистый прогноз модели")).toHaveTextContent("0.4 R");
});

it.each([
  { timezone: "America/New_York", ticks: ["30.12", "31.12", "03.01"], first: "30.12.2025" },
  { timezone: "Asia/Kathmandu", ticks: ["31.12", "01.01", "04.01"], first: "31.12.2025" },
])("uses DD.MM across the year with full accessible/tooltip dates in $timezone, and re-presents without refetch", async ({ timezone, ticks, first }) => {
  const DateTimeFormat = Intl.DateTimeFormat;
  vi.spyOn(Intl, "DateTimeFormat").mockImplementation(function (locales, options) { return new DateTimeFormat(locales, { ...options, timeZone: options?.timeZone ?? "Asia/Tokyo" }); });
  applyInterfaceSettings({ timezone });
  const fetch = mock(forecastFixture(request));
  render(<AnalyticsForecastChart request={request} days={days} />);
  const curve = await screen.findByTestId("forecast-model-curve");
  const chart = screen.getByRole("img");
  const labels = [...chart.querySelectorAll('text[y="273"]')];
  expect(labels.map((label) => label.firstChild?.textContent)).toEqual(ticks);
  expect(labels[0]).toHaveAccessibleName(first);
  expect(labels[0]!.querySelector("title")).toHaveTextContent(first);
  expect(observedCircles(chart)[0]!.querySelector("title")).toHaveTextContent(`${first} (${timezone})`);
  const before = curve.getAttribute("points");
  act(() => { applyInterfaceSettings({ timezone: "Asia/Kathmandu", dateOrder: "ymd", dateSeparator: "-", yearDigits: 4 }); });
  expect(screen.getByRole("img")).toBe(chart);
  expect(observedCircles(chart)[0]!.querySelector("title")).toHaveTextContent("2025-12-31 (Asia/Kathmandu)");
  expect(curve.getAttribute("points")).toBe(before); // No DST in this period.
  expect(JSON.parse(fetch.mock.calls[0]![1]!.body as string)).toEqual(request);
  expect(fetch).toHaveBeenCalledTimes(1);
});

it.each([
  { start: "2026-03-08T00:00:00Z", hours: 23 },
  { start: "2026-11-01T00:00:00Z", hours: 25 },
])("uses DST-safe New York presentation days ($hours hours), never local midnights as API cohorts", async ({ start, hours }) => {
  applyInterfaceSettings({ timezone: "America/New_York" });
  const first = Date.parse(start) / 1000;
  const observed = [0, 1, 2].map((offset) => ({ ...days[0]!, day: `day-${offset}`, dayStartUnix: first + offset * 86400 }));
  const input = { ...request, fromUnix: first, toUnix: first + 3 * 86400, observationCutoff: first + 4 * 86400, atUnix: first + 5 * 86400,
    daily: observed.map((point) => ({ dayStartUnix: point.dayStartUnix, tpCount: point.tpCount, slCount: point.slCount })) };
  const fetch = mock(forecastFixture(input));
  render(<AnalyticsForecastChart request={input} days={observed} />);
  await screen.findByTestId("forecast-model-curve");
  const chart = screen.getByRole("img");
  const grid = within(chart).getByTestId("daily-calendar-grid");
  const lines = [...grid.querySelectorAll("line")];
  const boundaries = lines.map((line) => Number(line.getAttribute("data-day-start-unix")));
  expect(boundaries).toEqual(observed.map((point) => zonedDayStart(point.dayStartUnix)));
  expect((boundaries[2]! - boundaries[1]!) / 3600).toBe(hours);
  observedCircles(chart).forEach((circle, index) => expect(circle.getAttribute("cx")).toBe(lines[index]!.getAttribute("x1")));
  expect(JSON.parse(fetch.mock.calls[0]![1]!.body as string)).toEqual(input);
  expect(input.daily.every((point) => point.dayStartUnix % 86400 === 0)).toBe(true);
  expect(boundaries.some((day) => day % 86400 !== 0)).toBe(true);
});

it("preserves unavailable/null model gaps without zero substitution or a bridge, and does not label a missing endpoint", async () => {
  const response = forecastFixture(request);
  response.curve[1]!.forecastR = null; response.blendCurve![1]!.decisionR = null;
  mock(withDisplayCurve(request, response));
  render(<AnalyticsForecastChart request={request} days={days} />);
  await screen.findByTestId("forecast-endpoint-label");
  expect(screen.queryByTestId("forecast-model-curve")).not.toBeInTheDocument();
  expect(screen.getAllByTestId("forecast-model-point")).toHaveLength(2);
  expect(screen.queryByTestId("forecast-blend-curve")).not.toBeInTheDocument();
  expect(screen.getAllByTestId("forecast-blend-point")).toHaveLength(2);
  cleanup();
  const unavailable = forecastFixture(request);
  unavailable.estimate = { status: "unavailable", reason: "no_closed_outcomes" };
  unavailable.meanR = null; unavailable.forecastR = null; unavailable.decisionR = null;
  unavailable.curve.forEach((point) => { point.forecastR = null; });
  unavailable.blendCurve!.forEach((point) => { point.decisionR = null; });
  unavailable.flatFuture.forecastR = null; unavailable.flatFuture.decisionR = null;
  mock(withDisplayCurve(request, unavailable));
  render(<AnalyticsForecastChart request={request} days={days} />);
  await screen.findByText(/Прогноз недоступен: нет наблюдавшихся/);
  expect(screen.queryByTestId("forecast-endpoint-label")).not.toBeInTheDocument();
  expect(screen.queryByTestId("forecast-model-point")).not.toBeInTheDocument();
});

it("retains the accepted complete display while a same-scope iteration fits, and observed sum does not refetch", async () => {
  const nextDays = [...days, { ...days[0]!, day: "next", dayStartUnix: fromUnix + 5 * 86400 }];
  const nextRequest = { ...request, daily: nextDays.map((day) => ({ dayStartUnix: day.dayStartUnix, tpCount: day.tpCount, slCount: day.slCount })) };
  let finish: ((response: Response) => void) | undefined;
  const fetch = vi.fn<typeof globalThis.fetch>().mockResolvedValueOnce(new Response(JSON.stringify(forecastFixture(request))))
    .mockImplementationOnce(() => new Promise<Response>((resolve) => { finish = resolve; }));
  vi.stubGlobal("fetch", fetch);
  const view = render(<AnalyticsForecastChart retainWhileLoading request={request} days={days} />);
  const curve = await screen.findByTestId("forecast-model-curve");
  const chart = screen.getByRole("img"); const before = chart.innerHTML;
  view.rerender(<AnalyticsForecastChart retainWhileLoading request={nextRequest} days={nextDays} />);
  await waitFor(() => expect(finish).toBeDefined());
  expect(screen.getByTestId("forecast-model-curve")).toBe(curve); expect(chart.innerHTML).toBe(before);
  act(() => { finish!(new Response(JSON.stringify(forecastFixture(nextRequest)))); });
  await waitFor(() => expect(observedCircles(chart)).toHaveLength(4));
  fireEvent.change(screen.getByLabelText("Фактический ряд по дням"), { target: { value: "sum" } });
  expect(fetch).toHaveBeenCalledTimes(2);
});

it("keeps the legacy long UTC grid bounded with a tile", () => {
  render(<svg><DailyCalendarGrid fromUnix={0} toUnix={4000 * 86400} x={(at) => at / 86400} top={0} bottom={100} /></svg>);
  const grid = screen.getByTestId("daily-calendar-grid");
  expect(grid.querySelectorAll("line")).toHaveLength(0); expect(grid.querySelector("pattern")).toHaveAttribute("width", "1");
  expect(grid.querySelectorAll("*").length).toBeLessThan(6);
});
