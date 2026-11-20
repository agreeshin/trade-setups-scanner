import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import { Alert, AlertDescription } from "../components/ui/alert";
import { Badge } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import { Card, CardContent } from "../components/ui/card";
import { Checkbox } from "../components/ui/checkbox";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "../components/ui/dialog";
import { EmptyState } from "../components/ui/empty-state";
import { Field } from "../components/ui/field";
import { InfoNote } from "../components/ui/info-note";
import { Input } from "../components/ui/input";
import { Select } from "../components/ui/select";
import { Spinner } from "../components/ui/spinner";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "../components/ui/table";
import { fetchPlatform } from "../lib/platform-api";
import { interfaceSettings, nextZonedDay, subscribeInterfaceSettings, zonedDateLabel, zonedDayStart } from "../lib/datetime-format";
import { getOptimizationStudy, type OptimizationMatrixCell, type OptimizationStudy } from "../lib/optimization-api";
import {
  fetchSetupsAnalytics,
  fetchSetupsAnalyticsStudies,
  fetchStudyExpectancy,
  type SetupsAnalyticsResult,
  type SetupsAnalyticsSetup,
  type SetupsAnalyticsStudyMeta,
  type StudyExpectancyPoint,
  type StudyExpectancySeries,
  type StudyTrendPoint,
} from "../lib/setups-analytics-api";
import {
  algorithmDisplayName,
  endOfRangeDateChange,
  errorText,
  localDateInput,
  localTimezoneLabel,
  replayRange,
  utcMidnightOnDateChange,
  type Algorithm,
} from "./platform-controls";

import { formatOutcomeRisk as formatRisk, outcomeMeanClass as meanColorClass, SetupOutcomesTable } from "./setup-outcomes-table";
import { calendarDays, CHART_STYLE } from "./study-chart-style";

const FILTER_STORAGE_KEY = "setups-analytics-filter-v1";

interface StoredFilter {
  algorithmId: string;
  from: string;
  to: string;
  studies: number[] | null;
}

function loadStoredFilter(): StoredFilter | null {
  try {
    const raw = window.localStorage.getItem(FILTER_STORAGE_KEY);
    if (raw === null) return null;
    const value: unknown = JSON.parse(raw);
    if (value === null || typeof value !== "object" || Array.isArray(value)) return null;
    const record = value as Record<string, unknown>;
    if (typeof record.algorithmId !== "string" || typeof record.from !== "string" || typeof record.to !== "string") return null;
    const studies: unknown = record.studies;
    if (studies !== null && (!Array.isArray(studies) || !studies.every((item) => typeof item === "number" && Number.isSafeInteger(item)))) return null;
    return { algorithmId: record.algorithmId, from: record.from, to: record.to, studies: studies as number[] | null };
  } catch {
    return null;
  }
}

function storeFilter(filter: StoredFilter): void {
  try {
    window.localStorage.setItem(FILTER_STORAGE_KEY, JSON.stringify(filter));
  } catch {
    return;
  }
}

function formatSignedR(value: number): string {
  return `${value >= 0 ? "+" : ""}${value.toFixed(2)} R`;
}

function GateBadge({ passed, selected }: { passed: number; selected: number }) {
  return <Badge className="bg-ambient-violet/15 text-ambient-violet" aria-label={`Прошли гейт: ${passed} из ${selected}`}>{passed}/{selected}</Badge>;
}

const DAY_SECONDS = 86400;

interface DailyOutcome {
  dayUnix: number;
  closedCount: number;
  meanR: number;
}

interface AnalyticsRange {
  fromUnix: number;
  toUnix: number;
}

interface CalculatedResult extends SetupsAnalyticsResult {
  range: AnalyticsRange;
}

function dailyOutcomes(setups: readonly SetupsAnalyticsSetup[], dayStart: (unix: number) => number = zonedDayStart): DailyOutcome[] {
  const byDay = new Map<number, { sum: number; count: number }>();
  for (const setup of setups) {
    if ((setup.status !== "tp" && setup.status !== "sl") || setup.entryTimeUnix === null || setup.riskReturn === null) continue;
    const day = dayStart(setup.entryTimeUnix);
    const bucket = byDay.get(day) ?? { sum: 0, count: 0 };
    bucket.sum += setup.riskReturn;
    bucket.count += 1;
    byDay.set(day, bucket);
  }
  return [...byDay.entries()]
    .map(([dayUnix, bucket]) => ({ dayUnix, closedCount: bucket.count, meanR: bucket.sum / bucket.count }))
    .sort((left, right) => left.dayUnix - right.dayUnix);
}

function DailyOutcomesChart({ setups, range }: { setups: readonly SetupsAnalyticsSetup[]; range: AnalyticsRange }) {
  const outcomes = dailyOutcomes(setups);
  const firstDay = Math.min(zonedDayStart(range.fromUnix), outcomes[0]?.dayUnix ?? Infinity);
  // The API interval is [from, to): an exact midnight end does not add an empty next day.
  // Selection is by raw signal, so a shifted entry can also extend beyond the selected interval.
  const lastDay = Math.max(zonedDayStart(range.toUnix - 1), outcomes.at(-1)?.dayUnix ?? -Infinity);
  const timeSpan = Math.max(nextZonedDay(firstDay) - firstDay, lastDay - firstDay);
  const xOf = (dayUnix: number) => CHART_STYLE.left + ((dayUnix - firstDay) / timeSpan) * (CHART_STYLE.right - CHART_STYLE.left);
  const allDays = calendarDays(firstDay, lastDay);
  const values = outcomes.map((point) => point.meanR);
  const rawMin = Math.min(0, ...values);
  const rawMax = Math.max(0, ...values);
  const pad = rawMax - rawMin < 1e-9 ? 0.5 : (rawMax - rawMin) * 0.08;
  const yMin = rawMin - pad;
  const yMax = rawMax + pad;
  const yOf = (value: number) => CHART_STYLE.bottom - ((value - yMin) / (yMax - yMin)) * (CHART_STYLE.bottom - CHART_STYLE.top);
  const labelStep = Math.max(1, Math.ceil((allDays.length - 1) / 8));
  const gridDays = allDays.filter((_, index) => index % labelStep === 0 || index === allDays.length - 1);
  const gridLevels: number[] = [];
  for (let level = Math.ceil(yMin); level <= Math.floor(yMax); level += 1) {
    if (level !== 0) gridLevels.push(level);
  }
  return <svg data-testid="setups-daily-outcomes" data-from-unix={range.fromUnix} data-to-unix={range.toUnix} viewBox={`0 0 ${CHART_STYLE.width} ${CHART_STYLE.height}`} role="img" aria-label="Средний результат закрытых сделок по дням входа" className="w-full rounded-md border border-input/60">
    <rect width={CHART_STYLE.width} height={CHART_STYLE.height} fill={CHART_STYLE.background} />
    <text x={CHART_STYLE.width / 2} y={28} textAnchor="middle" fontSize={14} fill={CHART_STYLE.foreground}>Результаты торговли по дням</text>
    {gridDays.map((dayUnix) => <g key={`day-${dayUnix}`} data-grid-day={dayUnix}>
      <line x1={xOf(dayUnix)} y1={CHART_STYLE.top} x2={xOf(dayUnix)} y2={CHART_STYLE.bottom} stroke={CHART_STYLE.grid} strokeWidth={1} />
      <text x={xOf(dayUnix)} y={CHART_STYLE.bottom + 16} textAnchor="middle" fontSize={11} fill={CHART_STYLE.axis}>{zonedDateLabel(dayUnix)}</text>
    </g>)}
    {gridLevels.map((level) => <g key={`level-${level}`}>
      <line x1={CHART_STYLE.left} y1={yOf(level)} x2={CHART_STYLE.right} y2={yOf(level)} stroke={CHART_STYLE.grid} strokeWidth={1} />
      <text x={CHART_STYLE.left - 8} y={yOf(level) + 4} textAnchor="end" fontSize={11} fill={CHART_STYLE.axis}>{level} R</text>
    </g>)}
    <line x1={CHART_STYLE.left} y1={yOf(0)} x2={CHART_STYLE.right} y2={yOf(0)} stroke={CHART_STYLE.axis} strokeWidth={1.5} />
    <text x={CHART_STYLE.left - 8} y={yOf(0) + 4} textAnchor="end" fontSize={11} fill={CHART_STYLE.axis}>0 R</text>
    {outcomes.length > 1 && <polyline fill="none" stroke={CHART_STYLE.line} strokeWidth={2} points={outcomes.map((point) => `${xOf(point.dayUnix)},${yOf(point.meanR)}`).join(" ")} />}
    {outcomes.length === 0 && <text x={CHART_STYLE.width / 2} y={CHART_STYLE.top + 20} textAnchor="middle" fontSize={12} fill={CHART_STYLE.axis}>Нет закрытых сделок для графика по дням.</text>}
    {outcomes.map((point) => <circle key={`point-${point.dayUnix}`} cx={xOf(point.dayUnix)} cy={yOf(point.meanR)} r={3} fill={point.meanR >= 0 ? CHART_STYLE.positive : CHART_STYLE.negative} data-day={zonedDateLabel(point.dayUnix)} data-closed={point.closedCount} data-mean-r={point.meanR.toFixed(2)}>
      <title>{`${zonedDateLabel(point.dayUnix)}: ${formatRisk(point.meanR)} · закрытых ${point.closedCount}`}</title>
    </circle>)}
  </svg>;
}

function bestMeanPair(matrix: OptimizationMatrixCell[]): { tpPct: number; slPct: number; meanR: number } | null {
  let best: { tpPct: number; slPct: number; meanR: number } | null = null;
  for (const cell of matrix) {
    const value = cell.meanRiskExpectancy;
    if (value === null || value === undefined || !Number.isFinite(value)) continue;
    if (best === null || value > best.meanR) best = { tpPct: cell.tpPct, slPct: cell.slPct, meanR: value };
  }
  return best;
}

function studyPointWithMean(point: StudyExpectancyPoint): point is StudyExpectancyPoint & { meanR: number } {
  return point.meanR !== null;
}

function trendValueAt(points: readonly StudyTrendPoint[], unix: number): number | null {
  if (points.length === 0) return null;
  const first = points[0]!;
  if (unix <= first.dayUnix) return first.value;
  const last = points.at(-1)!;
  if (unix >= last.dayUnix) return last.value;
  for (let index = 1; index < points.length; index += 1) {
    const point = points[index]!;
    if (point.dayUnix >= unix) {
      const previous = points[index - 1]!;
      const share = (unix - previous.dayUnix) / (point.dayUnix - previous.dayUnix);
      return previous.value + (point.value - previous.value) * share;
    }
  }
  return last.value;
}

function StudyHorizonChart({ series, live, dividerDay, nowUnix }: { series: StudyExpectancySeries; live: readonly DailyOutcome[]; dividerDay: number | null; nowUnix: number }) {
  // Legacy daily means/counts stay frozen: map each original bucket instant to its display day only.
  const allPast = series.daily.filter(studyPointWithMean).map((point) => ({ ...point, dayUnix: zonedDayStart(point.dayUnix) }));
  const releasePast = dividerDay === null ? null : allPast.find((point) => point.dayUnix === dividerDay) ?? null;
  const releaseLive = dividerDay === null ? null : live.find((point) => point.dayUnix === dividerDay) ?? null;
  const releaseClosed = (releasePast !== null ? releasePast.tpCount + releasePast.slCount : 0) + (releaseLive !== null ? releaseLive.closedCount : 0);
  const releaseMean = releaseClosed > 0
    ? ((releasePast !== null ? releasePast.meanR * (releasePast.tpCount + releasePast.slCount) : 0) + (releaseLive !== null ? releaseLive.meanR * releaseLive.closedCount : 0)) / releaseClosed
    : null;
  const past = dividerDay === null || releasePast === null ? allPast : allPast.filter((point) => point.dayUnix !== dividerDay);
  const livePoints = dividerDay === null || releaseLive === null ? live : live.filter((point) => point.dayUnix !== dividerDay);
  if (past.length === 0 && livePoints.length === 0 && releaseMean === null) {
    return <p className="text-sm text-muted-foreground">Нет данных для графика.</p>;
  }
  const releaseLineValue = releaseMean ?? releasePast?.meanR ?? releaseLive?.meanR ?? null;
  const linePoints = [...past, ...(dividerDay !== null && releaseLineValue !== null ? [{ dayUnix: dividerDay, meanR: releaseLineValue }] : []), ...livePoints].sort((a, b) => a.dayUnix - b.dayUnix);
  const trendVisible = series.trend.points.filter((point) => point.dayUnix <= nowUnix);
  const trendEndValue = trendValueAt(series.trend.points, nowUnix);
  const trendLine = trendVisible.length > 0 && trendEndValue !== null ? [...trendVisible, { dayUnix: nowUnix, value: trendEndValue }] : [];
  const dayNumbers = linePoints.map((point) => point.dayUnix);
  if (dividerDay !== null) dayNumbers.push(dividerDay);
  dayNumbers.push(nowUnix);
  const firstDay = zonedDayStart(Math.min(...dayNumbers));
  const lastUnix = Math.max(...dayNumbers);
  const timeSpan = Math.max(nextZonedDay(firstDay) - firstDay, lastUnix - firstDay);
  // Model points and forecast-now are exact instants, unlike the calendar-day observation buckets.
  const xOf = (unix: number) => CHART_STYLE.left + ((unix - firstDay) / timeSpan) * (CHART_STYLE.right - CHART_STYLE.left);
  const values = [...linePoints.map((point) => point.meanR), ...trendLine.map((point) => point.value)];
  const rawMin = Math.min(0, ...values);
  const rawMax = Math.max(0, ...values);
  const pad = rawMax - rawMin < 1e-9 ? 0.5 : (rawMax - rawMin) * 0.08;
  const yMin = rawMin - pad;
  const yMax = rawMax + pad;
  const yOf = (value: number) => CHART_STYLE.bottom - ((value - yMin) / (yMax - yMin)) * (CHART_STYLE.bottom - CHART_STYLE.top);
  const allDays = calendarDays(firstDay, zonedDayStart(lastUnix));
  const labelStep = Math.max(1, Math.ceil((allDays.length - 1) / 8));
  const labeledDays = allDays.filter((_, index) => index % labelStep === 0 || index === allDays.length - 1);
  const gridLevels: number[] = [];
  for (let level = Math.ceil(yMin); level <= Math.floor(yMax); level += 1) {
    if (level !== 0) gridLevels.push(level);
  }
  const dividerX = dividerDay === null ? null : Math.min(Math.max(xOf(dividerDay), CHART_STYLE.left), CHART_STYLE.right);
  return <svg data-testid="study-horizon-chart" viewBox={`0 0 ${CHART_STYLE.width} ${CHART_STYLE.height}`} role="img" aria-label="Матожидание: ретроспектива исследования и live-сетапы" className="w-full rounded-md border border-input/60">
    <rect width={CHART_STYLE.width} height={CHART_STYLE.height} fill={CHART_STYLE.background} />
    <text x={CHART_STYLE.width / 2} y={28} textAnchor="middle" fontSize={14} fill={CHART_STYLE.foreground}>Динамика матожидания: ретроспектива и live</text>
    {allDays.map((dayUnix) => <line key={`grid-${dayUnix}`} className="grid-day" data-grid-day={dayUnix} x1={xOf(dayUnix)} y1={CHART_STYLE.top} x2={xOf(dayUnix)} y2={CHART_STYLE.bottom} stroke={CHART_STYLE.grid} strokeWidth={1} />)}
    {labeledDays.map((dayUnix) => <text key={`date-${dayUnix}`} className="date" x={xOf(dayUnix)} y={CHART_STYLE.bottom + 16} textAnchor="middle" fontSize={11} fill={CHART_STYLE.axis}>{zonedDateLabel(dayUnix)}</text>)}
    {gridLevels.map((level) => <g key={`level-${level}`}>
      <line x1={CHART_STYLE.left} y1={yOf(level)} x2={CHART_STYLE.right} y2={yOf(level)} stroke={CHART_STYLE.grid} strokeWidth={1} />
      <text x={CHART_STYLE.left - 8} y={yOf(level) + 4} textAnchor="end" fontSize={11} fill={CHART_STYLE.axis}>{level} R</text>
    </g>)}
    <line x1={CHART_STYLE.left} y1={yOf(0)} x2={CHART_STYLE.right} y2={yOf(0)} stroke={CHART_STYLE.axis} strokeWidth={1.5} />
    <text x={CHART_STYLE.left - 8} y={yOf(0) + 4} textAnchor="end" fontSize={11} fill={CHART_STYLE.axis}>0 R</text>
    {trendLine.length > 1 && <polyline data-testid="forecast-line" fill="none" stroke={CHART_STYLE.trend} strokeWidth={2} data-method={series.trend.method ?? undefined} points={trendLine.map((point) => `${xOf(point.dayUnix)},${yOf(point.value)}`).join(" ")} />}
    {trendLine.length > 1 && (() => {
      const endX = xOf(nowUnix);
      const endY = yOf(trendLine.at(-1)!.value);
      const anchor = endX > CHART_STYLE.right - 60 ? "end" : endX < CHART_STYLE.left + 60 ? "start" : "middle";
      return <text data-testid="forecast-now-value" x={endX} y={Math.max(endY - 8, CHART_STYLE.top + 12)} textAnchor={anchor} fontSize={11} fontWeight={600} fill={CHART_STYLE.trend}>{formatRisk(trendLine.at(-1)!.value)}</text>;
    })()}
    {linePoints.length > 1 && <polyline data-testid="expectancy-line" fill="none" stroke={CHART_STYLE.line} strokeWidth={2} points={linePoints.map((point) => `${xOf(point.dayUnix)},${yOf(point.meanR)}`).join(" ")} />}
    {past.map((point) => <circle key={`past-${point.dayUnix}`} cx={xOf(point.dayUnix)} cy={yOf(point.meanR)} r={3} fill={point.meanR >= 0 ? CHART_STYLE.positive : CHART_STYLE.negative} data-day={zonedDateLabel(point.dayUnix)} data-phase="past" data-closed={point.tpCount + point.slCount} data-mean-r={point.meanR.toFixed(2)}>
      <title>{`${zonedDateLabel(point.dayUnix)}: ${formatRisk(point.meanR)} · ретроспектива`}</title>
    </circle>)}
    {livePoints.map((point) => <circle key={`live-${point.dayUnix}`} cx={xOf(point.dayUnix)} cy={yOf(point.meanR)} r={3} fill={point.meanR >= 0 ? CHART_STYLE.positive : CHART_STYLE.negative} data-day={zonedDateLabel(point.dayUnix)} data-phase="live" data-closed={point.closedCount} data-mean-r={point.meanR.toFixed(2)}>
      <title>{`${zonedDateLabel(point.dayUnix)}: ${formatRisk(point.meanR)} · закрытых ${point.closedCount}`}</title>
    </circle>)}
    {dividerDay !== null && releaseMean !== null && <circle key="release" cx={xOf(dividerDay)} cy={yOf(releaseMean)} r={4} fill={releaseMean >= 0 ? CHART_STYLE.positive : CHART_STYLE.negative} data-day={zonedDateLabel(dividerDay)} data-phase="release" data-closed={releaseClosed} data-mean-r={releaseMean.toFixed(2)}>
      <title>{`День выпуска исследования: ${formatRisk(releaseMean)} · закрытых ${releaseClosed}`}</title>
    </circle>}
    {dividerX !== null && <g data-divider-day={dividerDay}>
      <line x1={dividerX} y1={CHART_STYLE.top} x2={dividerX} y2={CHART_STYLE.bottom} stroke={CHART_STYLE.axis} strokeWidth={1.5} strokeDasharray="3 4" />
      <text x={dividerX > CHART_STYLE.width / 2 ? dividerX - 6 : dividerX + 6} y={45} textAnchor={dividerX > CHART_STYLE.width / 2 ? "end" : "start"} fontSize={11} fill={CHART_STYLE.axis}>Старт исследования</text>
    </g>}
  </svg>;
}

function paramFieldLabel(fields: Algorithm["fields"], key: string): string {
  const field = fields.find((item) => item.key === key);
  if (field === undefined) return key;
  return field.unit === "" ? field.label : `${field.label} (${field.unit})`;
}

function StudyDetailsDialog({ studyId, algorithmId, fields, meta, onClose }: { studyId: number | null; algorithmId: string; fields: Algorithm["fields"]; meta: SetupsAnalyticsStudyMeta | undefined; onClose: () => void }) {
  // The dialog is keyed by study ID: preserve its opening-time endpoint through display-setting changes.
  const [nowUnix] = useState(() => Math.floor(Date.now() / 1000));
  const [study, setStudy] = useState<OptimizationStudy | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [series, setSeries] = useState<StudyExpectancySeries | null>(null);
  const [chartError, setChartError] = useState<string | null>(null);
  // Keep the bounded raw response so a timezone change can regroup a mounted chart without refitting.
  const [live, setLive] = useState<SetupsAnalyticsSetup[] | null>(null);

  useEffect(() => {
    if (studyId === null) return;
    const controller = new AbortController();
    void getOptimizationStudy(studyId, controller.signal)
      .then((value) => { if (!controller.signal.aborted) setStudy(value); })
      .catch((cause) => { if (!controller.signal.aborted) setLoadError(errorText(cause)); });
    return () => controller.abort();
  }, [studyId]);

  useEffect(() => {
    if (studyId === null || study === null) return;
    const controller = new AbortController();
    const startedUnix = Math.floor(Date.parse(study.createdAt) / 1000);
    const fromUnix = Number.isFinite(startedUnix) && startedUnix > 0
      ? Math.floor(startedUnix / DAY_SECONDS) * DAY_SECONDS
      : Math.floor(Date.now() / 1000 / DAY_SECONDS) * DAY_SECONDS - 30 * DAY_SECONDS;
    void (async () => {
      let outcomes: DailyOutcome[];
      try {
        const value = await fetchSetupsAnalytics(algorithmId, fromUnix, Math.floor(Date.now() / 1000), [studyId], controller.signal);
        // The server model still consumes UTC-day aggregates; only the displayed raw trades regroup.
        outcomes = dailyOutcomes(value.setups, (unix) => Math.floor(unix / DAY_SECONDS) * DAY_SECONDS);
        if (!controller.signal.aborted) setLive(value.setups);
      } catch (cause) {
        if (!controller.signal.aborted) setChartError(`Не удалось получить live-исходы для графика: ${errorText(cause)}`);
        return;
      }
      try {
        const series = await fetchStudyExpectancy(studyId, outcomes.map((point) => ({ dayUnix: point.dayUnix, meanR: point.meanR, closedCount: point.closedCount })), controller.signal);
        if (!controller.signal.aborted) setSeries(series);
      } catch (cause) {
        if (!controller.signal.aborted) setChartError(errorText(cause));
      }
    })();
    return () => controller.abort();
  }, [studyId, study, algorithmId]);

  const dividerDay = (() => {
    if (study !== null) {
      const startedUnix = Math.floor(Date.parse(study.createdAt) / 1000);
      if (Number.isFinite(startedUnix) && startedUnix > 0) return zonedDayStart(startedUnix);
    }
    return series !== null ? zonedDayStart(series.observationCutoff) : null;
  })();

  const best = study?.result?.best ?? null;
  const trendMode = best?.trendMode ?? study?.spec.trendMode;
  const forecastPair = study?.forecastBest ?? null;
  const meanPair = best === null ? null : bestMeanPair(best.matrix);
  const pairKindLabel = trendMode === "blend" ? "Лучшая blend-пара" : trendMode === undefined || trendMode === "mean" ? "Лучшая mean-пара" : "Лучшая forecast-пара";
  const pair = trendMode === "blend" || trendMode === "forecast"
    ? forecastPair !== null
      ? { text: `TP ${forecastPair.tpPct}% / SL ${forecastPair.slPct}% · ${formatSignedR(forecastPair.forecastR)}` }
      : meanPair !== null ? { text: `TP ${meanPair.tpPct}% / SL ${meanPair.slPct}% · ${formatSignedR(meanPair.meanR)}` } : null
    : meanPair !== null ? { text: `TP ${meanPair.tpPct}% / SL ${meanPair.slPct}% · ${formatSignedR(meanPair.meanR)}` } : null;
  const score = best?.score ?? study?.progress.bestScore ?? null;

  return <Dialog open={studyId !== null} onOpenChange={(open) => { if (!open) onClose(); }}>
    {studyId !== null && <DialogContent className="max-w-5xl" aria-label={`Исследование №${studyId}`}>
      <DialogHeader>
        <DialogTitle>Исследование №{studyId}</DialogTitle>
        <DialogDescription>{meta?.externalName ?? "Подробности исследования и график динамики матожидания."}</DialogDescription>
      </DialogHeader>
      {loadError !== null && <p role="alert" className="text-sm text-danger">{loadError}</p>}
      {study === null && loadError === null && <p className="text-sm text-muted-foreground">Загрузка подробностей…</p>}
      {study !== null && <div className="grid gap-4">
        <dl className="grid gap-2 text-sm sm:grid-cols-2">
          <div><dt className="text-muted-foreground">Лучший score</dt><dd className="tabular-nums">{score === null ? "—" : formatSignedR(score)}</dd></div>
          <div><dt className="text-muted-foreground">{pairKindLabel}</dt><dd className="tabular-nums">{pair === null ? "Не сохранено" : pair.text}</dd></div>
        </dl>
        <div>
          <h4 className="font-medium">Полные параметры алгоритма</h4>
          {best === null
            ? <p className="mt-1 text-sm text-muted-foreground">Не сохранено</p>
            : <dl className="mt-2 grid gap-2 text-sm sm:grid-cols-2">{Object.entries(best.params).map(([key, value]) => <div key={key} className="break-words"><dt className="text-muted-foreground">{paramFieldLabel(fields, key)}</dt><dd>{String(value)}</dd></div>)}</dl>}
        </div>
        <div>
          <h4 className="font-medium">Аналитические параметры</h4>
          {best?.analytics === undefined
            ? <p className="mt-1 text-sm text-muted-foreground">Не сохранено</p>
            : <dl className="mt-2 grid gap-2 text-sm sm:grid-cols-2">
              <div><dt className="text-muted-foreground">Сдвиг времени, мин</dt><dd className="tabular-nums">{best.analytics.timeShiftMinutes}</dd></div>
              <div><dt className="text-muted-foreground">Смещение цены, %</dt><dd className="tabular-nums">{best.analytics.priceChangePct === null || best.analytics.priceChangePct === undefined ? "не используется" : String(best.analytics.priceChangePct)}</dd></div>
              <div><dt className="text-muted-foreground">Кластеризация</dt><dd>{best.analytics.clusterEnabled ? "включена" : "выключена"}</dd></div>
              <div><dt className="text-muted-foreground">Зрелость сетапа в кластере</dt><dd className="tabular-nums">{best.analytics.setupMaturity === null || best.analytics.setupMaturity === undefined ? "—" : best.analytics.setupMaturity.toFixed(1)}</dd></div>
              <div><dt className="text-muted-foreground">Разрыв кластера, ч</dt><dd className="tabular-nums">{best.analytics.clusterGapMinutes === null || best.analytics.clusterGapMinutes === undefined ? "—" : String(best.analytics.clusterGapMinutes / 60)}</dd></div>
              <div><dt className="text-muted-foreground">Горизонт сделки, мин</dt><dd className="tabular-nums">{best.analytics.horizonMinutes === undefined ? "—" : String(best.analytics.horizonMinutes)}</dd></div>
            </dl>}
        </div>
      </div>}
      <div className="grid gap-2">
        <h4 className="font-medium">Динамика матожидания: ретроспектива и live</h4>
        {chartError !== null && <p role="alert" className="text-sm text-muted-foreground">{chartError}</p>}
        {chartError === null && series === null && <p className="text-sm text-muted-foreground">Загрузка графика…</p>}
        {series !== null && chartError === null && <StudyHorizonChart series={series} live={dailyOutcomes(live ?? [])} dividerDay={dividerDay} nowUnix={nowUnix} />}
      </div>
    </DialogContent>}
  </Dialog>;
}

function StudyPickerDialog({ open, studies, selection, onSelectionChange, onClose, onApply }: {
  open: boolean;
  studies: SetupsAnalyticsStudyMeta[];
  selection: ReadonlySet<number>;
  onSelectionChange: (studyId: number, checked: boolean) => void;
  onClose: () => void;
  onApply: () => void;
}) {
  return <Dialog open={open} onOpenChange={(next) => { if (!next) onClose(); }}>
    {open && <DialogContent className="max-w-xl" aria-label="Исследования-основы, выпустившие сетапы">
      <DialogHeader>
        <DialogTitle>Исследования-основы, выпустившие сетапы</DialogTitle>
        <DialogDescription>Список зависит от диапазона выпуска сетапов и обновляется при смене дат.</DialogDescription>
      </DialogHeader>
      <div className="flex flex-wrap gap-2">
        <Button variant="outline" onClick={() => { for (const study of studies) onSelectionChange(study.studyId, true); }}>Выбрать все</Button>
        <Button variant="outline" onClick={() => { for (const study of studies) onSelectionChange(study.studyId, false); }}>Снять все</Button>
      </div>
      <div className="max-h-96 overflow-y-auto">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Выбран</TableHead>
              <TableHead>Исследование</TableHead>
              <TableHead numeric>Сетапов</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {studies.map((study) => <TableRow key={study.studyId}>
              <TableCell><Checkbox checked={selection.has(study.studyId)} aria-label={`Исследование ${study.studyId}`} onCheckedChange={(checked) => onSelectionChange(study.studyId, checked === true)} /></TableCell>
              <TableCell className="font-medium">№{study.studyId}</TableCell>
              <TableCell numeric>{study.setupCount}</TableCell>
            </TableRow>)}
          </TableBody>
        </Table>
      </div>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-xs text-muted-foreground">Выбрано: {selection.size} из {studies.length}</p>
        <div className="flex gap-2">
          <Button variant="outline" onClick={onClose}>Отмена</Button>
          <Button onClick={onApply}>Готово</Button>
        </div>
      </div>
    </DialogContent>}
  </Dialog>;
}

function initialAlgorithmId(): string {
  return loadStoredFilter()?.algorithmId ?? "";
}

function initialRangeInputs(): { from: string; to: string } {
  const stored = loadStoredFilter();
  return {
    from: stored?.from ?? localDateInput(Math.floor(Date.now() / 1000) - 7 * 86400),
    to: stored?.to ?? localDateInput(Math.floor(Date.now() / 1000)),
  };
}

function initialSelectedStudies(): ReadonlySet<number> | null {
  const studies = loadStoredFilter()?.studies;
  return studies ? new Set(studies) : null;
}

export function SetupsAnalyticsPanel() {
  useSyncExternalStore(subscribeInterfaceSettings, interfaceSettings, interfaceSettings);
  const [algorithms, setAlgorithms] = useState<Algorithm[] | null>(null);
  const [algorithmId, setAlgorithmId] = useState(initialAlgorithmId);
  const [fromInput, setFromInput] = useState(() => initialRangeInputs().from);
  const [toInput, setToInput] = useState(() => initialRangeInputs().to);
  const [committed, setCommitted] = useState(initialRangeInputs);
  const [studiesMeta, setStudiesMeta] = useState<SetupsAnalyticsStudyMeta[]>([]);
  const [studiesState, setStudiesState] = useState<"idle" | "loading" | "ready" | "error">("idle");
  const [studiesError, setStudiesError] = useState<string | null>(null);
  const [selectedStudies, setSelectedStudies] = useState<ReadonlySet<number> | null>(initialSelectedStudies);
  const [pickerOpen, setPickerOpen] = useState(false);
  const [pickerSelection, setPickerSelection] = useState<ReadonlySet<number>>(new Set());
  const [detailStudyId, setDetailStudyId] = useState<number | null>(null);
  const [result, setResult] = useState<CalculatedResult | null>(null);
  const [running, setRunning] = useState(false);
  const [hideGateFailed, setHideGateFailed] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const calculateRef = useRef<AbortController | null>(null);
  const restoredSelection = useRef<ReadonlySet<number> | null>(initialSelectedStudies());

  const range = (() => {
    try {
      return { value: replayRange(fromInput, toInput), error: null as string | null };
    } catch (cause) {
      return { value: null, error: errorText(cause) };
    }
  })();
  const committedRange = (() => {
    try {
      return replayRange(committed.from, committed.to);
    } catch {
      return null;
    }
  })();
  const committedFrom = committedRange?.fromUnix ?? null;
  const committedTo = committedRange?.toUnix ?? null;
  const studiesStatus = algorithmId === "" || committedFrom === null || committedTo === null ? "idle" : studiesState;

  useEffect(() => {
    const controller = new AbortController();
    void fetchPlatform<{ algorithms: Algorithm[] }>("/v1/algorithms", controller.signal).then((state) => {
      if (controller.signal.aborted) return;
      const list = state.kind === "ready" && Array.isArray(state.data.algorithms) ? state.data.algorithms : [];
      setAlgorithms(list);
      setAlgorithmId((current) => list.some((algorithm) => algorithm.id === current) ? current : (list[0]?.id ?? ""));
      if (state.kind === "unreachable") setStudiesError(`Список алгоритмов недоступен (${state.detail})`);
    });
    return () => controller.abort();
  }, []);

  useEffect(() => {
    if (algorithmId === "" || committedFrom === null || committedTo === null) return;
    const controller = new AbortController();
    const from = committedFrom;
    const to = committedTo;
    async function load() {
      setStudiesState("loading");
      try {
        const items = await fetchSetupsAnalyticsStudies(algorithmId, from, to, controller.signal);
        if (controller.signal.aborted) return;
        setStudiesMeta(items);
        setStudiesState("ready");
        setStudiesError(null);
        const restored = restoredSelection.current;
        if (restored === null) {
          setSelectedStudies(null);
        } else {
          const intersection = new Set(items.filter((item) => restored.has(item.studyId)).map((item) => item.studyId));
          setSelectedStudies(intersection.size === 0 ? null : intersection);
          restoredSelection.current = null;
        }
      } catch (cause) {
        if (controller.signal.aborted) return;
        setStudiesMeta([]);
        setStudiesState("error");
        setStudiesError(errorText(cause));
        setSelectedStudies(null);
        restoredSelection.current = null;
      }
    }
    void load();
    return () => controller.abort();
  }, [algorithmId, committedFrom, committedTo]);

  useEffect(() => {
    storeFilter({
      algorithmId,
      from: fromInput,
      to: toInput,
      studies: selectedStudies === null ? null : [...selectedStudies].sort((left, right) => left - right),
    });
  }, [algorithmId, fromInput, toInput, selectedStudies]);

  useEffect(() => () => calculateRef.current?.abort(), []);

  function commitDates() {
    setCommitted({ from: fromInput, to: toInput });
  }

  function openPicker() {
    setPickerSelection(selectedStudies === null ? new Set(studiesMeta.map((study) => study.studyId)) : new Set(selectedStudies));
    setPickerOpen(true);
  }

  function togglePickerStudy(studyId: number, checked: boolean) {
    setPickerSelection((current) => {
      const next = new Set(current);
      if (checked) next.add(studyId);
      else next.delete(studyId);
      return next;
    });
  }

  async function run() {
    if (running || algorithmId === "" || range.value === null) return;
    calculateRef.current?.abort();
    const controller = new AbortController();
    calculateRef.current = controller;
    setRunning(true);
    setError(null);
    try {
      const studies = selectedStudies === null ? null : [...selectedStudies].sort((left, right) => left - right);
      const requestedRange = { ...range.value };
      const data = await fetchSetupsAnalytics(algorithmId, requestedRange.fromUnix, requestedRange.toUnix, studies, controller.signal);
      if (!controller.signal.aborted) setResult({ ...data, range: requestedRange });
    } catch (cause) {
      if (!controller.signal.aborted) setError(errorText(cause));
    } finally {
      if (calculateRef.current === controller) {
        calculateRef.current = null;
        setRunning(false);
      }
    }
  }

  const studiesLabel = selectedStudies === null
    ? `Все (${studiesMeta.length})`
    : `${selectedStudies.size} из ${studiesMeta.length}`;
  const detailMeta = detailStudyId === null ? undefined : studiesMeta.find((study) => study.studyId === detailStudyId);

  return <section aria-label="Аналитика выпущенных сетапов" className="grid min-w-0 gap-4">
    <div className="flex items-center gap-2">
      <h2 className="text-lg font-semibold">Аналитика выпущенных сетапов</h2>
      <InfoNote>Расчёт по сохранённым рекомендациям исследований: вход по рекомендованному времени и гейту смещения цены, исход TP/SL по реальным минутным свечам за 7 суток. Исход хранится в самом сетапе; используются сохранённые результаты, рассчитываются только недостающие.</InfoNote>
    </div>
    <Card>
      <CardContent className="flex flex-wrap items-end gap-3">
        <Field label="Алгоритм" className="w-full max-w-xs">
          {(control) => <Select {...control} value={algorithmId} disabled={algorithms === null} onChange={(event) => { setAlgorithmId(event.target.value); setResult(null); }}>
            {(algorithms ?? []).map((algorithm) => <option key={algorithm.id} value={algorithm.id}>{algorithmDisplayName(algorithm)}</option>)}
          </Select>}
        </Field>
        <Field label="Начало (момент выхода сетапа)" className="w-60">
          {(control) => <Input {...control} type="datetime-local" value={fromInput} onBlur={commitDates} onChange={(event) => { setFromInput(utcMidnightOnDateChange(fromInput, event.target.value)); setResult(null); }} />}
        </Field>
        <Field label="Конец" className="w-60">
          {(control) => <Input {...control} type="datetime-local" value={toInput} onBlur={commitDates} onChange={(event) => { setToInput(endOfRangeDateChange(toInput, event.target.value)); setResult(null); }} />}
        </Field>
        <Button variant="outline" disabled={studiesMeta.length === 0} onClick={openPicker}>Исследования: {studiesLabel}</Button>
        <Button disabled={running || algorithmId === "" || range.value === null} onClick={() => { void run(); }}>{running ? "Расчёт…" : "Рассчитать"}</Button>
        {studiesStatus === "loading" && <Spinner label="Загрузка исследований…" />}
        <p className="w-full text-xs text-muted-foreground">Время: {localTimezoneLabel()}. Смена даты сбрасывает время к полуночи в выбранном часовом поясе (для «Конец» сегодняшняя дата подставляет текущее время); список исследований обновляется при потере фокуса полей дат, после обновления выбраны все. По нажатию кнопки используются сохранённые исходы и рассчитываются только недостающие.</p>
        {range.error !== null && <p role="alert" className="w-full text-xs text-danger">{range.error}</p>}
      </CardContent>
    </Card>
    {studiesError && <Alert variant="destructive" role="alert"><AlertDescription>{studiesError}</AlertDescription></Alert>}
    {error && <Alert variant="destructive" role="alert"><AlertDescription>{error}</AlertDescription></Alert>}
    {running && <Spinner label="Выполняется расчёт исходов сетапов…" />}
    {result && <Card>
      <CardContent className="grid gap-4">
        <div className="flex flex-wrap items-center gap-6">
          <div>
            <p className="text-sm text-muted-foreground">Среднее матожидание закрытых сделок</p>
            <p data-testid="setups-analytics-mean" className={`text-4xl font-semibold tabular-nums ${meanColorClass(result.totals.meanR)}`}>{formatRisk(result.totals.meanR)}</p>
            <p className="text-xs text-muted-foreground">закрытых сделок: {result.totals.closed}</p>
          </div>
          <div className="flex flex-col items-start gap-1">
            <span className="text-xs text-muted-foreground">Прошли гейт / выбрано</span>
            <GateBadge passed={result.totals.gatePassed} selected={result.totals.selected} />
          </div>
        </div>
        <DailyOutcomesChart setups={result.setups} range={result.range} />
        {result.studies.length > 0 && <div className="grid gap-2">
          <h3 className="text-sm font-semibold text-muted-foreground">Исследования-основы</h3>
          <div className="overflow-x-auto rounded-md border border-input/60">
            <Table>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  <TableHead>Исследование</TableHead>
                  <TableHead>Прошли гейт / выбрано</TableHead>
                  <TableHead numeric>Матожидание</TableHead>
                  <TableHead numeric>Закрытых</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {result.studies.map((study) => <TableRow key={study.studyId}>
                  <TableCell>
                    <Button type="button" variant="outline" size="sm" aria-label={`Подробности исследования ${study.studyId}`} onClick={() => setDetailStudyId(study.studyId)}>#{study.studyId}</Button>
                  </TableCell>
                  <TableCell><GateBadge passed={study.gatePassed} selected={study.selected} /></TableCell>
                  <TableCell numeric className={`font-medium ${meanColorClass(study.meanR)}`}>{formatRisk(study.meanR)}</TableCell>
                  <TableCell numeric><Badge variant="warning">{study.closed}/{study.gatePassed}</Badge></TableCell>
                </TableRow>)}
              </TableBody>
            </Table>
          </div>
        </div>}
        {result.setups.length === 0
          ? <EmptyState title="Нет сетапов по выбранным фильтрам" description="Измените диапазон, алгоритм или набор исследований." />
          : <div className="grid gap-2">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <h3 className="text-sm font-semibold text-muted-foreground">Сетапы <span className="font-normal text-muted-foreground/80">{hideGateFailed ? result.setups.filter((setup) => setup.status !== "gate_failed").length : result.setups.length}/{result.setups.length}</span></h3>
              <label className="flex items-center gap-2 text-sm text-muted-foreground"><Checkbox checked={hideGateFailed} onCheckedChange={(checked) => setHideGateFailed(checked === true)} /> Скрыть сетапы, которые не прошли гейт</label>
            </div>
            <SetupOutcomesTable setups={hideGateFailed ? result.setups.filter((setup) => setup.status !== "gate_failed") : result.setups} />
          </div>}
      </CardContent>
    </Card>}
    <StudyPickerDialog
      open={pickerOpen}
      studies={studiesMeta}
      selection={pickerSelection}
      onSelectionChange={togglePickerStudy}
      onClose={() => setPickerOpen(false)}
      onApply={() => { setSelectedStudies(pickerSelection.size === studiesMeta.length ? null : new Set(pickerSelection)); setPickerOpen(false); }}
    />
    <StudyDetailsDialog key={detailStudyId ?? "none"} studyId={detailStudyId} algorithmId={algorithmId} fields={(algorithms ?? []).find((algorithm) => algorithm.id === algorithmId)?.fields ?? []} meta={detailMeta} onClose={() => setDetailStudyId(null)} />
  </section>;
}
