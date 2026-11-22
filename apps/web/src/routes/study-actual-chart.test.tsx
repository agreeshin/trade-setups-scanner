import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { actualCutoff, actualDay, actualReportFixture, actualStart, havanaActualDetailFixture } from "../../test/study-actual-fixture";
import { applyInterfaceSettings, DEFAULT_INTERFACE_SETTINGS } from "../lib/datetime-format";
import { getStudyActual, type OriginalObjectiveDTO } from "../lib/study-actual-api";
import { StudyActualChart } from "./study-actual-chart";
import { CHART_STYLE } from "./study-chart-style";

beforeEach(() => { applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "UTC" }); });
afterEach(() => { cleanup(); vi.unstubAllGlobals(); applyInterfaceSettings(DEFAULT_INTERFACE_SETTINGS); });

it("uses the reference study chart palette and responsive SVG geometry", () => {
  render(<StudyActualChart report={actualReportFixture()} />);
  const chart = screen.getByTestId("study-actual-chart");
  expect(chart).toHaveAttribute("viewBox", "0 0 900 320"); expect(chart).toHaveClass("w-full", "rounded-md", "border-input/60");
  expect(screen.getByTestId("study-original-objective-line")).toHaveAttribute("stroke", CHART_STYLE.trend);
  expect(chart).toHaveAttribute("data-cutoff", String(actualCutoff));
});

it("renders one server-weighted start-day observation on the exact same grid coordinate as the start marker", () => {
  render(<StudyActualChart report={actualReportFixture()} />);
  const chart = screen.getByTestId("study-actual-chart");
  const points = chart.querySelectorAll(`circle[data-phase="observed"][data-day-unix="${actualDay}"]`);
  expect(points).toHaveLength(1);
  const actual = points[0]!;
  const divider = screen.getByTestId("study-execution-divider").querySelector("line")!;
  expect(actual).toHaveAttribute("data-mean-r", "0.2"); expect(actual).toHaveAttribute("data-sum-r", "1");
  expect(actual).toHaveAttribute("data-closed", "5");
  expect(actual.getAttribute("cx")).toBe(divider.getAttribute("x1"));
  expect(chart.querySelector(`[data-grid-day="${actualDay}"]`)).toHaveAttribute("x1", actual.getAttribute("cx"));
  expect(screen.getByTestId("study-observed-line")).toBeInTheDocument();
  expect(screen.getByTestId("study-execution-divider").querySelector("title")).toHaveTextContent("2026-11-20T12:00:00.125Z");
});

it.each([
  { version: 1, trendMode: "mean", trendBendDays: "linear", trendBlendLambda: 0.7, forecast: null },
  { version: 1, trendMode: "forecast", trendBendDays: "14", trendBlendLambda: 0.7, forecast: null },
  { version: 1, trendMode: "forecast", trendBendDays: "linear", trendBlendLambda: 0.7, forecast: { method: "decayed_probability", version: 1, halfLifeDays: 14, kappa: 10, priorPolicy: "gross_break_even" } },
  { version: 1, trendMode: "blend", trendBendDays: "linear", trendBlendLambda: 0.3, forecast: { method: "trend", version: 1, bendDays: "linear" } },
] satisfies OriginalObjectiveDTO[])("renders server-provided original objective %j without deriving a new forecast", (originalObjective) => {
  const report = actualReportFixture({ originalObjective });
  const view = render(<StudyActualChart report={report} />);
  expect(screen.getByTestId("study-actual-chart")).toHaveAttribute("data-objective", originalObjective.trendMode);
  expect(screen.getByTestId("study-original-end-value")).toHaveTextContent("+0.80R");
  const originalProjection = JSON.stringify(report.projection);
  view.rerender(<StudyActualChart report={{ ...report, display: { ...report.display!, daily: [{ ...report.display!.daily[0]!, meanR: -19, sumR: -76 }] } }} />);
  expect(screen.getByTestId("study-original-end-value")).toHaveTextContent("+0.80R");
  expect(JSON.stringify(report.projection)).toBe(originalProjection);
});

it("keeps the Havana 04:30Z signal on the first-midnight server day through GET parsing, grid lookup and observed line", async () => {
  const body = havanaActualDetailFixture(); const timeZone = "America/Havana";
  act(() => { applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: timeZone }); });
  const fetch = vi.fn().mockResolvedValue(new Response(JSON.stringify(body), { status: 200 })); vi.stubGlobal("fetch", fetch);
  const detail = await getStudyActual(49, new AbortController().signal, timeZone);
  expect(detail.report!.display!.timeZone).toBe(timeZone);
  const day = Date.parse("2026-11-01T04:00:00Z") / 1000;
  const signal = detail.report!.actual.daily[0]!.plotAtUnix;
  const signalDay = new Intl.DateTimeFormat("en-CA", { timeZone, year: "numeric", month: "2-digit", day: "2-digit" }).format(signal * 1000);
  expect(signalDay).toBe("2026-11-01");
  render(<StudyActualChart report={detail.report!} />);
  const chart = screen.getByTestId("study-actual-chart");
  const points = chart.querySelectorAll(`circle[data-phase="observed"][data-day-unix="${day}"]`);
  expect(points).toHaveLength(1);
  const point = points[0]!;
  expect(point).toHaveAttribute("data-closed", "5"); expect(point).toHaveAttribute("data-mean-r", "0.2");
  const coordinate = point.getAttribute("cx");
  expect(screen.getByTestId("study-execution-divider").querySelector("line")).toHaveAttribute("x1", coordinate);
  expect(chart.querySelector(`[data-grid-day="${day}"]`)).toHaveAttribute("x1", coordinate);
  expect(screen.getByTestId("study-observed-line")).toHaveAttribute("points", expect.stringContaining(`${coordinate},${point.getAttribute("cy")}`));
  const nextDay = Date.parse("2026-11-02T05:00:00Z") / 1000;
  expect(chart.querySelector(`[data-grid-day="${nextDay}"]`)).toBeInTheDocument();
  expect(nextDay - day).toBe(25 * 3600);
  expect(chart.querySelector(`[data-axis-day="${day}"]`)).toHaveTextContent("01.11");
  expect(point.querySelector("title")).toHaveTextContent("2026-11-01");
});

it("does not bridge null forecast gaps, zero-fill observations, or hide the trained yellow line when no post-start trades closed", () => {
  const report = actualReportFixture({ historical: { availability: { status: "unavailable", reason: "no_original_outcomes" }, daily: [] }, actual: { daily: [{ dayUnix: actualDay, plotAtUnix: actualStart, meanR: null, closedCount: 0, tpCount: 0, slCount: 0 }] } });
  render(<StudyActualChart report={{ ...report,
    projection: { ...report.projection, points: [{ atUnix: actualDay - 86400, value: 0.2 }, { atUnix: actualDay, value: null }, { atUnix: actualCutoff, value: 0.8 }] } }} />);
  expect(screen.getAllByTestId("study-original-objective-segment")).toHaveLength(2);
  expect(screen.queryByTestId("study-original-objective-line")).not.toBeInTheDocument();
  expect(screen.getByTestId("study-original-end-value")).toHaveTextContent("+0.80R");
  expect(screen.getByTestId("study-actual-chart").querySelector('circle[data-phase="observed"]')).toBeNull();
});

it.each([
  ["America/New_York", "2026-11-01T17:00:00.125Z", "2026-11-01T04:00:00Z", "2026-11-02T05:00:00Z", "2026-11-01", "2026-11-02", 90000],
  ["Asia/Kathmandu", "2026-11-20T01:00:00.125Z", "2026-11-19T18:15:00Z", "2026-11-20T18:15:00Z", "2026-11-20", "2026-11-21", 86400],
] as const)("aligns exact %s local days, points and marker using real Unix spacing", (timeZone, executionStartedAt, firstIso, secondIso, firstDate, secondDate, distance) => {
  act(() => { applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: timeZone }); });
  const first = Date.parse(firstIso) / 1000; const second = Date.parse(secondIso) / 1000;
  const base = actualReportFixture();
  render(<StudyActualChart report={{ ...base, executionStartedAt, observedThroughUnix: second + 3600,
    projection: { ...base.projection, points: [{ atUnix: first, value: 0.2 }, { atUnix: second, value: 0.8 }] },
    display: { ...base.display!, timeZone, startDayUnix: first, daily: [
      { dayUnix: first, date: firstDate, sumR: 2, meanR: 1, closedCount: 2, tpCount: 2, slCount: 0 },
      { dayUnix: second, date: secondDate, sumR: -1, meanR: -1, closedCount: 1, tpCount: 0, slCount: 1 },
    ] } }} />);
  expect(second - first).toBe(distance);
  const chart = screen.getByTestId("study-actual-chart");
  const marker = screen.getByTestId("study-execution-divider");
  expect(marker.querySelector("line")).toHaveAttribute("x1", chart.querySelector(`[data-grid-day="${first}"]`)!.getAttribute("x1"));
  expect(chart.querySelector(`[data-day-unix="${first}"]`)).toHaveAttribute("cx", marker.querySelector("line")!.getAttribute("x1"));
  expect(marker.querySelector("title")).toHaveTextContent(executionStartedAt);
  expect(chart.querySelector(`[data-day-unix="${second}"] title`)).toHaveTextContent(secondDate);
});

it.each(["original-only", "merged"])("connects sparse %s finite observations with real time spacing and no invented points/tooltips", (source) => {
  const base = actualReportFixture({ ...(source === "original-only" ? { actual: { daily: [] } } : {}) });
  const first = base.display!.daily[0]!;
  const daily = [first,
    { ...first, dayUnix: actualDay, date: "2026-11-20", meanR: null, sumR: null, closedCount: 0, tpCount: 0, slCount: 0 },
    { ...first, dayUnix: actualDay + 2 * 86400, date: "2026-11-22", meanR: -1, sumR: -4 },
    { ...first, dayUnix: actualDay + 3 * 86400, date: "2026-11-23", meanR: 0, sumR: 0 }];
  const report = { ...base, observedThroughUnix: actualDay + 4 * 86400, display: { ...base.display!, daily } };
  const sealedProjection = JSON.stringify(report.projection); const originalDays = JSON.stringify(daily);
  render(<StudyActualChart report={report} />);
  const chart = screen.getByTestId("study-actual-chart");
  const points = [...chart.querySelectorAll('circle[data-phase="observed"]')];
  expect(points).toHaveLength(3);
  expect(points.map((point) => point.getAttribute("data-date"))).toEqual(["2026-11-19", "2026-11-22", "2026-11-23"]);
  expect(screen.getAllByTestId("study-observed-line")).toHaveLength(1);
  expect(screen.getByTestId("study-observed-line").getAttribute("points")?.split(" ")).toEqual(points.map((point) => `${point.getAttribute("cx")},${point.getAttribute("cy")}`));
  const xs = points.map((point) => Number(point.getAttribute("cx")));
  expect((xs[1]! - xs[0]!) / (xs[2]! - xs[1]!)).toBeCloseTo(3);
  expect(chart.querySelector(`[data-grid-day="${actualDay + 86400}"]`)).toBeInTheDocument();
  expect(chart.querySelector(`[data-day-unix="${actualDay + 86400}"]`)).toBeNull();
  expect(chart.querySelector(`[data-day-unix="${actualDay}"]`)).toBeNull();
  expect(chart.querySelectorAll('circle[data-phase="observed"] title')).toHaveLength(3);
  expect(JSON.stringify(report.projection)).toBe(sealedProjection); expect(JSON.stringify(daily)).toBe(originalDays);
});

it.each([[], [0.5], [null, NaN, Infinity], [1, NaN, -1]].map((values) => ({ values })))("draws only finite observations $values and requires two points for an observed segment", ({ values }) => {
  const base = actualReportFixture();
  render(<StudyActualChart report={{ ...base, observedThroughUnix: actualDay + 4 * 86400,
    display: { ...base.display!, daily: values.map((meanR, index) => ({ ...base.display!.daily[0]!, dayUnix: actualDay + index * 86400, date: `2026-11-${20 + index}`, meanR })) } }} />);
  const finiteCount = values.filter((value) => value !== null && Number.isFinite(value)).length;
  expect(screen.getByTestId("study-actual-chart").querySelectorAll('circle[data-phase="observed"]')).toHaveLength(finiteCount);
  expect(screen.queryAllByTestId("study-observed-line")).toHaveLength(finiteCount > 1 ? 1 : 0);
  expect(screen.getByTestId("study-actual-chart").innerHTML).not.toMatch(/(?:cx|cy|points)="[^"]*(?:NaN|Infinity)/);
});

it("uses only DD.MM axis labels through a local year crossing, retaining full accessible and tooltip dates", () => {
  act(() => { applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "Asia/Kathmandu", dateOrder: "mdy", yearDigits: 2 }); });
  const first = Date.parse("2026-12-30T18:15:00Z") / 1000; const second = first + 86400;
  const base = actualReportFixture();
  render(<StudyActualChart report={{ ...base, executionStartedAt: "2026-12-31T20:00:00.125Z", observedThroughUnix: second + 86400 + 100,
    projection: { ...base.projection, points: [{ atUnix: first, value: 0.2 }, { atUnix: second, value: 0.8 }] },
    display: { ...base.display!, timeZone: "Asia/Kathmandu", startDayUnix: second, daily: [{ ...base.display!.daily[0]!, dayUnix: second, date: "2027-01-01" }] } }} />);
  const labels = screen.getByTestId("study-actual-chart").querySelectorAll("[data-axis-day]");
  expect([...labels].map((label) => label.textContent)).toEqual(["31.12", "01.01", "02.01"]);
  expect(labels[0]).toHaveAttribute("aria-label", "31.12.2026 (Asia/Kathmandu)");
  expect(labels[1]).toHaveAttribute("aria-label", "01.01.2027 (Asia/Kathmandu)");
  expect(screen.getByTestId("study-actual-chart").querySelector('circle[data-phase="observed"] title')).toHaveTextContent("2027-01-01");
});

it("keeps at most nine compact date ticks on a narrow forty-day chart", () => {
  vi.stubGlobal("innerWidth", 375);
  render(<StudyActualChart report={actualReportFixture({ observedThroughUnix: actualDay + 40 * 86400 })} />);
  const chart = screen.getByTestId("study-actual-chart");
  const labels = chart.querySelectorAll("[data-axis-day]");
  expect(labels.length).toBeLessThanOrEqual(9);
  expect([...labels].every((label) => /^\d{2}\.\d{2}$/.test(label.textContent ?? ""))).toBe(true);
  expect(chart).toHaveClass("w-full");
});
