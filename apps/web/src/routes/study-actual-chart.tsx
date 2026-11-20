import { useSyncExternalStore } from "react";
import { interfaceSettings, nextZonedDay, subscribeInterfaceSettings, zonedDateLabel, zonedDayStart } from "../lib/datetime-format";
import type { StudyActualReport } from "../lib/study-actual-api";
import { formatOutcomeRisk } from "./setup-outcomes-table";
import { calendarDays, CHART_STYLE as C } from "./study-chart-style";

interface PlotPoint { at: number; value: number | null }
/** Null is a real gap, not a zero or permission to connect the neighboring observations. */
function segments(points: readonly PlotPoint[]): PlotPoint[][] {
  const lines: PlotPoint[][] = []; let line: PlotPoint[] = [];
  for (const point of points) {
    if (point.value === null) { if (line.length > 0) lines.push(line); line = []; }
    else line.push(point);
  }
  if (line.length > 0) lines.push(line);
  return lines;
}

/** Geometry only. All values, clipped point instants and original-objective forecasts are server-owned. */
export function StudyActualChart({ report }: { report: StudyActualReport }) {
  useSyncExternalStore(subscribeInterfaceSettings, interfaceSettings);
  const start = Date.parse(report.executionStartedAt) / 1000;
  const cutoff = report.observedThroughUnix;
  const past = report.historical.availability.status === "available" ? report.historical.daily : [];
  const projection = report.projection.availability.status === "available" ? report.projection.points : [];
  const actual = report.actual.daily;
  const observed = [...past, ...actual];
  const first = zonedDayStart(Math.min(start, cutoff, ...observed.map((point) => point.plotAtUnix), ...projection.map((point) => point.atUnix)));
  const last = Math.max(start, cutoff);
  const span = Math.max(nextZonedDay(first) - first, last - first);
  const x = (at: number) => C.left + ((at - first) / span) * (C.right - C.left);
  const values = [...observed.map((point) => point.meanR), ...projection.map((point) => point.value)].filter((value): value is number => value !== null);
  const low = Math.min(0, ...values); const high = Math.max(0, ...values);
  const pad = high - low < 1e-9 ? 0.5 : (high - low) * 0.08;
  const min = low - pad; const max = high + pad;
  const y = (value: number) => C.bottom - ((value - min) / (max - min)) * (C.bottom - C.top);
  const days = calendarDays(first, zonedDayStart(last));
  const labelStep = Math.max(1, Math.ceil((days.length - 1) / 8));
  const levels: number[] = [];
  const levelStep = Math.max(1, Math.ceil((max - min) / 20));
  for (let level = Math.ceil(min / levelStep) * levelStep; level <= Math.floor(max); level += levelStep) if (level !== 0) levels.push(level);
  const path = (points: PlotPoint[]) => points.map((point) => `${x(point.at)},${y(point.value!)}`).join(" ");
  const renderObserved = (points: typeof actual, phase: "past" | "actual") => <g data-phase-segment={phase}>
    {segments(points.map((point) => ({ at: point.plotAtUnix, value: point.meanR }))).map((line, index) => line.length > 1 && <polyline key={index} data-testid={`study-${phase}-line`} fill="none" stroke={C.line} strokeWidth={2} points={path(line)} />)}
    {points.filter((point) => point.meanR !== null).map((point) => <circle key={`${point.dayUnix}:${point.plotAtUnix}`} data-phase={phase} data-day-unix={point.dayUnix} data-plot-at-unix={point.plotAtUnix} data-mean-r={point.meanR} data-closed={point.closedCount}
      cx={x(point.plotAtUnix)} cy={y(point.meanR!)} r={3} fill={point.meanR! >= 0 ? C.positive : C.negative}>
      <title>{`${zonedDateLabel(point.dayUnix)}: ${formatOutcomeRisk(point.meanR)} · ${phase === "past" ? "исходная история" : "после исследования"} · закрытых ${point.closedCount}`}</title>
    </circle>)}
  </g>;
  return <svg data-testid="study-actual-chart" data-execution-start={start} data-cutoff={cutoff} data-objective={report.originalObjective?.trendMode} data-method={report.projection.method ?? undefined}
    viewBox={`0 0 ${C.width} ${C.height}`} role="img" aria-label="Матожидание: исходное исследование и новое историческое воспроизведение" className="w-full rounded-md border border-input/60">
    <rect width={C.width} height={C.height} fill={C.background} />
    <text x={C.width / 2} y={28} textAnchor="middle" fontSize={14} fill={C.foreground}>Динамика матожидания: исследование и новый факт</text>
    {days.map((day, index) => <g key={day}>
      <line data-grid-day={day} x1={x(day)} y1={C.top} x2={x(day)} y2={C.bottom} stroke={C.grid} strokeWidth={1} />
      {(index % labelStep === 0 || index === days.length - 1) && <text x={x(day)} y={C.bottom + 16} textAnchor="middle" fontSize={11} fill={C.axis}>{zonedDateLabel(day)}</text>}
    </g>)}
    {levels.map((level) => <g key={level}><line x1={C.left} y1={y(level)} x2={C.right} y2={y(level)} stroke={C.grid} strokeWidth={1} /><text x={C.left - 8} y={y(level) + 4} textAnchor="end" fontSize={11} fill={C.axis}>{level} R</text></g>)}
    <line x1={C.left} y1={y(0)} x2={C.right} y2={y(0)} stroke={C.axis} strokeWidth={1.5} /><text x={C.left - 8} y={y(0) + 4} textAnchor="end" fontSize={11} fill={C.axis}>0 R</text>
    {segments(projection.map((point) => ({ at: point.atUnix, value: point.value }))).map((line, index) => <g key={index} data-testid="study-original-objective-segment">
      {line.length > 1 ? <polyline data-testid="study-original-objective-line" fill="none" stroke={C.trend} strokeWidth={2} points={path(line)} /> : <circle cx={x(line[0]!.at)} cy={y(line[0]!.value!)} r={2} fill={C.trend} />}
    </g>)}
    {projection.at(-1)?.value != null && <text data-testid="study-original-end-value" x={x(projection.at(-1)!.atUnix)} y={Math.max(y(projection.at(-1)!.value!) - 8, C.top + 12)} textAnchor="end" fontSize={11} fontWeight={600} fill={C.trend}>{formatOutcomeRisk(projection.at(-1)!.value)}</text>}
    {renderObserved(past, "past")}{renderObserved(actual, "actual")}
    <g data-testid="study-execution-divider"><line x1={x(start)} y1={C.top} x2={x(start)} y2={C.bottom} stroke={C.axis} strokeWidth={1.5} strokeDasharray="3 4" /><text x={x(start) > C.width / 2 ? x(start) - 6 : x(start) + 6} y={45} textAnchor={x(start) > C.width / 2 ? "end" : "start"} fontSize={11} fill={C.axis}>Старт исследования</text></g>
  </svg>;
}
