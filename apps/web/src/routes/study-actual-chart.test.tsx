import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, expect, it } from "vitest";
import { actualCutoff, actualDay, actualReportFixture, actualStart } from "../../test/study-actual-fixture";
import { applyInterfaceSettings, DEFAULT_INTERFACE_SETTINGS } from "../lib/datetime-format";
import type { OriginalObjectiveDTO } from "../lib/study-actual-api";
import { StudyActualChart } from "./study-actual-chart";
import { CHART_STYLE } from "./study-chart-style";

beforeEach(() => { applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "UTC" }); });
afterEach(() => { cleanup(); applyInterfaceSettings(DEFAULT_INTERFACE_SETTINGS); });

it("uses the reference study chart palette and responsive SVG geometry", () => {
  render(<StudyActualChart report={actualReportFixture()} />);
  const chart = screen.getByTestId("study-actual-chart");
  expect(chart).toHaveAttribute("viewBox", "0 0 900 320"); expect(chart).toHaveClass("w-full", "rounded-md", "border-input/60");
  expect(screen.getByTestId("study-original-objective-line")).toHaveAttribute("stroke", CHART_STYLE.trend);
  expect(chart).toHaveAttribute("data-cutoff", String(actualCutoff));
});

it("retains distinct original/post-start points on the same UTC day and never puts post-start geometry before S", () => {
  render(<StudyActualChart report={actualReportFixture()} />);
  const chart = screen.getByTestId("study-actual-chart");
  const past = chart.querySelector(`circle[data-phase="past"][data-day-unix="${actualDay}"]`)!;
  const actual = chart.querySelector('circle[data-phase="actual"]')!;
  const divider = screen.getByTestId("study-execution-divider").querySelector("line")!;
  expect(past).toHaveAttribute("data-mean-r", "-0.5"); expect(actual).toHaveAttribute("data-mean-r", "1.25");
  expect(actual).toHaveAttribute("data-day-unix", String(actualDay)); expect(actual).toHaveAttribute("data-plot-at-unix", String(actualStart));
  expect(Number(actual.getAttribute("cx"))).toBeGreaterThanOrEqual(Number(divider.getAttribute("x1")));
  expect(Number(past.getAttribute("cx"))).toBeLessThan(Number(divider.getAttribute("x1")));
  expect(screen.queryByTestId("study-actual-line")).not.toBeInTheDocument();
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
  view.rerender(<StudyActualChart report={{ ...report, actual: { daily: [{ ...report.actual.daily[0]!, meanR: -19 }] } }} />);
  expect(screen.getByTestId("study-original-end-value")).toHaveTextContent("+0.80R");
});

it("does not bridge null forecast gaps, zero-fill observations, or hide the trained yellow line when no post-start trades closed", () => {
  const report = actualReportFixture();
  render(<StudyActualChart report={{ ...report, actual: { daily: [{ ...report.actual.daily[0]!, meanR: null, closedCount: 0, tpCount: 0, slCount: 0 }] },
    projection: { ...report.projection, points: [{ atUnix: actualDay - 86400, value: 0.2 }, { atUnix: actualDay, value: null }, { atUnix: actualCutoff, value: 0.8 }] } }} />);
  expect(screen.getAllByTestId("study-original-objective-segment")).toHaveLength(2);
  expect(screen.queryByTestId("study-original-objective-line")).not.toBeInTheDocument();
  expect(screen.getByTestId("study-original-end-value")).toHaveTextContent("+0.80R");
  expect(screen.getByTestId("study-actual-chart").querySelector('circle[data-phase="actual"]')).toBeNull();
});

it("changes display labels with the configured timezone, not UTC cohort keys or numeric observations", () => {
  render(<StudyActualChart report={actualReportFixture()} />);
  const chart = screen.getByTestId("study-actual-chart"); const before = chart.querySelector('circle[data-phase="actual"]')!;
  expect(before.querySelector("title")).toHaveTextContent("20.11.2026");
  act(() => { applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "America/Los_Angeles" }); });
  const after = chart.querySelector('circle[data-phase="actual"]')!;
  expect(after).toHaveAttribute("data-day-unix", String(actualDay)); expect(after).toHaveAttribute("data-mean-r", "1.25");
  expect(after.querySelector("title")).toHaveTextContent("19.11.2026");
});
