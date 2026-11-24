import { useSyncExternalStore } from "react";
import { effectiveTimezone, formatIsoDateTime, interfaceSettings, nextZonedDay, subscribeInterfaceSettings, zonedDateLabel, zonedDayStart } from "../lib/datetime-format";
import type { StudyActualReport } from "../lib/study-actual-api";
import { formatOutcomeRisk } from "./setup-outcomes-table";
import { calendarDays, CHART_STYLE as C } from "./study-chart-style";

interface PlotPoint { at: number; value: number | null }
/** Preserve the sealed projection's explicit null gaps; observed data uses a separate sparse polyline. */
function segments(points: readonly PlotPoint[]): PlotPoint[][] {
  const lines: PlotPoint[][] = []; let line: PlotPoint[] = [];
  for (const point of points) {
    if (point.value === null) { if (line.length > 0) lines.push(line); line = []; }
    else line.push(point);
  }
  if (line.length > 0) lines.push(line);
  return lines;
}

/** Geometry only: exact local-day aggregation and sealed model values are server-owned. */
export function StudyActualChart({ report, combinedUnavailableReason }: { report: StudyActualReport; combinedUnavailableReason?: string }) {
  useSyncExternalStore(subscribeInterfaceSettings, interfaceSettings);
  const start = Date.parse(report.executionStartedAt) / 1000;
  const cutoff = report.observedThroughUnix;
  const display = report.display;
  const timeZone = display?.timeZone ?? effectiveTimezone();
  const tickFormatter = new Intl.DateTimeFormat("en-GB", { timeZone, day: "2-digit", month: "2-digit", year: "numeric" });
  const tickDate = (day: number, full = false) => {
    const parts = tickFormatter.formatToParts(day * 1000);
    return (full ? ["day", "month", "year"] : ["day", "month"]).map((part) => parts.find((item) => item.type === part)?.value).join(".");
  };
  const projection = report.projection.availability.status === "available" ? report.projection.points : [];
  const combined = report.combinedProjection;
  const combinedPoints = combinedUnavailableReason === undefined && combined?.availability.status === "available" ? combined.points : [];
  const combinedReason = combinedUnavailableReason ?? (combined?.availability.status === "unavailable" ? combined.availability.reason : combined === undefined ? "combined_projection_not_provided" : null);
  const observed = display?.daily ?? [];
  const startDay = display?.startDayUnix ?? zonedDayStart(start);
  const first = zonedDayStart(Math.min(startDay, cutoff, ...observed.map((point) => point.dayUnix), ...projection.map((point) => point.atUnix), ...combinedPoints.map((point) => point.atUnix)));
  const last = Math.max(start, cutoff);
  // The open report owns a bounded grid, never an unbounded calendar expansion.
  if (!Number.isFinite(first) || last - first > 3661 * 86400) return <p role="alert">Период графика превышает допустимые 3660 суток.</p>;
  const span = Math.max(nextZonedDay(first) - first, last - first);
  const x = (at: number) => C.left + ((at - first) / span) * (C.right - C.left);
  const values = [...observed.map((point) => point.meanR), ...projection.map((point) => point.value), ...combinedPoints.map((point) => point.value)].filter((value): value is number => value !== null && Number.isFinite(value));
  const low = Math.min(0, ...values); const high = Math.max(0, ...values);
  const pad = high - low < 1e-9 ? 0.5 : (high - low) * 0.08;
  const min = low - pad; const max = high + pad;
  const y = (value: number) => C.bottom - ((value - min) / (max - min)) * (C.bottom - C.top);
  const days = calendarDays(first, zonedDayStart(last));
  // Only real finite observations enter the line/markers; missing days remain grid-only.
  const availableObserved = observed.filter((point) => point.meanR !== null && Number.isFinite(point.meanR)).sort((left, right) => left.dayUnix - right.dayUnix);
  const observedPoints = availableObserved.map((point) => ({ at: point.dayUnix, value: point.meanR }));
  const labelStep = Math.max(1, Math.ceil((days.length - 1) / 8));
  const levels: number[] = [];
  const levelStep = Math.max(1, Math.ceil((max - min) / 20));
  for (let level = Math.ceil(min / levelStep) * levelStep; level <= Math.floor(max); level += levelStep) if (level !== 0) levels.push(level);
  const path = (points: PlotPoint[]) => points.map((point) => `${x(point.at)},${y(point.value!)}`).join(" ");
  return <><svg data-testid="study-actual-chart" data-execution-start={start} data-start-day={startDay} data-time-zone={display?.timeZone} data-cutoff={cutoff} data-objective={report.originalObjective?.trendMode} data-method={report.projection.method ?? undefined}
    viewBox={`0 0 ${C.width} ${C.height}`} role="img" aria-label="Матожидание: Исходная модель (пунктир), Модель по всем данным (сплошная) и наблюдаемые дневные результаты" className="w-full rounded-md border border-input/60">
    <rect width={C.width} height={C.height} fill={C.background} />
    <text x={C.width / 2} y={28} textAnchor="middle" fontSize={14} fill={C.foreground}>Динамика матожидания: исследование и новый факт</text>
    {days.map((day, index) => <g key={day}>
      <line data-grid-day={day} x1={x(day)} y1={C.top} x2={x(day)} y2={C.bottom} stroke={C.grid} strokeWidth={1} />
      {(index % labelStep === 0 || index === days.length - 1) && <text data-axis-day={day} aria-label={`${tickDate(day, true)} (${timeZone})`} x={x(day)} y={C.bottom + 16} textAnchor="middle" fontSize={11} fill={C.axis}>{tickDate(day)}</text>}
    </g>)}
    {levels.map((level) => <g key={level}><line x1={C.left} y1={y(level)} x2={C.right} y2={y(level)} stroke={C.grid} strokeWidth={1} /><text x={C.left - 8} y={y(level) + 4} textAnchor="end" fontSize={11} fill={C.axis}>{level} R</text></g>)}
    <line x1={C.left} y1={y(0)} x2={C.right} y2={y(0)} stroke={C.axis} strokeWidth={1.5} /><text x={C.left - 8} y={y(0) + 4} textAnchor="end" fontSize={11} fill={C.axis}>0 R</text>
    <g role="group" aria-label="Исходная модель (пунктир)">
      {segments(projection.map((point) => ({ at: point.atUnix, value: point.value }))).map((line, index) => <g key={index} data-testid="study-original-objective-segment">
        {line.length > 1 ? <polyline data-testid="study-original-objective-line" fill="none" stroke={C.trend} strokeWidth={2} strokeDasharray="6 4" points={path(line)} /> : <circle cx={x(line[0]!.at)} cy={y(line[0]!.value!)} r={2} fill={C.trend} />}
      </g>)}
    </g>
    <g role="group" aria-label="Модель по всем данным (сплошная)" data-combined-revision={combined?.revision} data-combined-cutoff={combined?.observationCutoff}>
      {segments(combinedPoints.map((point) => ({ at: point.atUnix, value: point.value }))).map((line, index) => <g key={index} data-testid="study-combined-objective-segment">
        {line.length > 1 ? <polyline data-testid="study-combined-objective-line" fill="none" stroke={C.trend} strokeWidth={2} points={path(line)} /> : <circle cx={x(line[0]!.at)} cy={y(line[0]!.value!)} r={2} fill={C.trend} />}
      </g>)}
    </g>
    {projection.at(-1)?.value != null && <text data-testid="study-original-end-value" x={x(projection.at(-1)!.atUnix)} y={Math.max(y(projection.at(-1)!.value!) - 8, C.top + 12)} textAnchor="end" fontSize={11} fontWeight={600} fill={C.trend}>{formatOutcomeRisk(projection.at(-1)!.value)}</text>}
    <g data-phase-segment="observed">
      {observedPoints.length > 1 && <polyline data-testid="study-observed-line" fill="none" stroke={C.line} strokeWidth={2} points={path(observedPoints)} />}
      {availableObserved.map((point) => <circle key={point.dayUnix} data-phase="observed" data-day-unix={point.dayUnix} data-date={point.date} data-sum-r={point.sumR} data-mean-r={point.meanR} data-closed={point.closedCount}
        cx={x(point.dayUnix)} cy={y(point.meanR!)} r={3} fill={point.meanR! >= 0 ? C.positive : C.negative}>
        <title>{`${zonedDateLabel(point.dayUnix)} (${point.date}, ${timeZone}): ${formatOutcomeRisk(point.meanR)} · закрытых ${point.closedCount} · TP ${point.tpCount} / SL ${point.slCount}`}</title>
      </circle>)}
    </g>
    <g data-testid="study-execution-divider"><title>{`Начало исполнения исследования: ${formatIsoDateTime(report.executionStartedAt)} (${display?.timeZone ?? ""}) · UTC ${report.executionStartedAt}`}</title><line x1={x(startDay)} y1={C.top} x2={x(startDay)} y2={C.bottom} stroke={C.axis} strokeWidth={1.5} strokeDasharray="3 4" /><text x={x(startDay) > C.width / 2 ? x(startDay) - 6 : x(startDay) + 6} y={45} textAnchor={x(startDay) > C.width / 2 ? "end" : "start"} fontSize={11} fill={C.axis}>Начало исполнения исследования</text></g>
  </svg>
    <div aria-label="Легенда модельных линий" className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-muted-foreground">
      <span className="inline-flex items-center gap-2"><svg aria-hidden="true" width="28" height="10"><line x1="0" y1="5" x2="28" y2="5" stroke={C.trend} strokeWidth="2" strokeDasharray="6 4" /></svg>Исходная модель · пунктир</span>
      <span className="inline-flex items-center gap-2"><svg aria-hidden="true" width="28" height="10"><line x1="0" y1="5" x2="28" y2="5" stroke={C.trend} strokeWidth="2" /></svg>Модель по всем данным · сплошная</span>
    </div>
    {combinedReason && <p data-testid="study-combined-unavailable" className="text-sm text-muted-foreground">Модель по всем данным недоступна: {combinedReason}. Исходная модель не переобучается.</p>}
  </>;
}
