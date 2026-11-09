import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Alert, AlertDescription } from "../components/ui/alert";
import { Badge } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import { Card, CardContent } from "../components/ui/card";
import { Checkbox } from "../components/ui/checkbox";
import { EmptyState } from "../components/ui/empty-state";
import { Field } from "../components/ui/field";
import { Input } from "../components/ui/input";
import { PageHeader } from "../components/ui/page-header";
import { SegmentedList, SegmentedTrigger } from "../components/ui/segmented";
import { Select } from "../components/ui/select";
import { Spinner } from "../components/ui/spinner";
import { parameterLabels } from "../lib/parameter-labels";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "../components/ui/table";
import {
  clearAnalyticsCache,
  fetchAnalyticsCacheSummary,
  fetchAnalyticsMonths,
  fetchAnalyticsSets,
  fetchMatrixCell,
  fetchMatrixCells,
  fetchAnalyticsSetups,
  fetchTradeRow,
  fetchTradeRows,
  type AnalyticsSet,
  type MatrixCellRef,
  type SelectionMetrics,
  type TradeRow,
} from "../lib/analytics-api";

const SHORT_MATRIX_LEVELS = Array.from({ length: 14 }, (_, index) => 4 + index * 2);
const LONG_TP_LEVELS = [1, 2, 3, ...SHORT_MATRIX_LEVELS, 35, 40, 45, 50, 60];
const LONG_SL_LEVELS = [0.1, 0.25, 0.5, 1, 2, 3, ...SHORT_MATRIX_LEVELS, 35, 40, 45, 50, 60];

function parseNumberInput(value: string): number | null {
  const normalized = value.trim().replace(",", ".");
  if (normalized === "") return null;
  const parsed = Number(normalized);
  return Number.isFinite(parsed) ? parsed : null;
}

function formatPrice(value: number | null): string {
  if (value === null) return "—";
  return String(Number(value.toPrecision(6)));
}

function formatRisk(value: number | null): string {
  if (value === null) return "—";
  return `${value >= 0 ? "+" : ""}${value.toFixed(2)}R`;
}

function formatTime(unix: number | null): string {
  if (unix === null) return "—";
  return new Date(unix * 1000).toLocaleString(undefined, { dateStyle: "short", timeStyle: "medium" });
}

function formatDuration(minutes: number | null): string {
  if (minutes === null) return "—";
  if (minutes < 60) return `${minutes.toFixed(0)} мин`;
  if (minutes < 60 * 24) return `${(minutes / 60).toFixed(1)} ч`;
  return `${(minutes / (60 * 24)).toFixed(1)} дн`;
}

/** Quantizes the value color to one of 21 gradient steps between the slider thresholds. */
function matrixCellTextStep(value: number, min: number, max: number): number {
  const span = max - min;
  const t = span > 0 ? Math.min(1, Math.max(0, (value - min) / span)) : value >= max ? 1 : 0;
  return Math.round(t * 20);
}

interface MatrixSectionProps {
  setId: string;
  description: string;
  direction: "short" | "long";
  tpLevels: number[];
  slLevels: number[];
  enablePriceChange: boolean;
  gradientMinDefault: number;
  gradientMaxDefault: number;
  clusterize: boolean;
  maturity: number;
  clusterGapHours: number | null;
  setBounds: { from: string; to: string } | null;
}

function MatrixSection({
  setId, description, direction, tpLevels, slLevels, enablePriceChange,
  gradientMinDefault, gradientMaxDefault, clusterize, maturity, clusterGapHours, setBounds,
}: MatrixSectionProps) {
  const [timeRange, setTimeRange] = useState<TimeRangeValue>({ mode: "all", month: "all", fromDay: null, toDay: null });
  const [timeShiftInput, setTimeShiftInput] = useState("0");
  const [considerPriceChange, setConsiderPriceChange] = useState(false);
  const [priceChangeInput, setPriceChangeInput] = useState("0");
  const [gradientMinDraft, setGradientMinDraft] = useState(gradientMinDefault);
  const [gradientMaxDraft, setGradientMaxDraft] = useState(gradientMaxDefault);
  const [gradientMin, setGradientMin] = useState(gradientMinDefault);
  const [gradientMax, setGradientMax] = useState(gradientMaxDefault);
  const [cells, setCells] = useState<Map<string, MatrixCellRef>>(new Map());
  const [metrics, setMetrics] = useState<SelectionMetrics | null>(null);
  const [running, setRunning] = useState(false);
  const [pendingCell, setPendingCell] = useState<string | null>(null);
  const [progress, setProgress] = useState({ done: 0, total: 0 });
  const [error, setError] = useState<string | null>(null);
  const runIdRef = useRef(0);

  const totalCells = tpLevels.length * slLevels.length;

  const stop = useCallback(() => {
    runIdRef.current += 1;
    setRunning(false);
    setPendingCell(null);
  }, []);

  const run = useCallback(async (shiftMinutes: number, priceChangePct: number | null) => {
    const runId = runIdRef.current + 1;
    runIdRef.current = runId;
    setRunning(true);
    setError(null);
    setPendingCell(null);
    const stale = () => runIdRef.current !== runId;
    const baseParams = {
      set: setId,
      direction,
      ...timeRangeParams(timeRange),
      ...(clusterize ? {
        setupMaturity: maturity,
        ...(clusterGapHours !== null && clusterGapHours > 0 ? { clusterGapHours } : {}),
      } : {}),
      ...(shiftMinutes > 0 ? { timeShiftMinutes: shiftMinutes } : {}),
      ...(priceChangePct !== null ? { priceChangePct } : {}),
    };
    try {
      const bulk = await fetchMatrixCells(baseParams);
      if (stale()) return;
      const nextCells = new Map(bulk.items.map((item) => [`${item.tpPct}:${item.slPct}`, item]));
      setCells(nextCells);
      setMetrics(bulk);
      setProgress({ done: nextCells.size, total: totalCells });
      for (const tpPct of tpLevels) {
        for (const slPct of slLevels) {
          const key = `${tpPct}:${slPct}`;
          if (nextCells.has(key)) continue;
          setPendingCell(key);
          const cell = await fetchMatrixCell({ ...baseParams, tpPct, slPct });
          if (stale()) return;
          nextCells.set(key, { tpPct, slPct, avgMovePct: cell.avgMovePct, riskExpectancy: cell.riskExpectancy });
          setCells(new Map(nextCells));
          setProgress({ done: nextCells.size, total: totalCells });
        }
      }
    } catch (cause) {
      if (!stale()) setError(cause instanceof Error ? cause.message : "ошибка расчёта");
    } finally {
      if (!stale()) {
        setRunning(false);
        setPendingCell(null);
      }
    }
  }, [setId, direction, timeRange, clusterize, maturity, clusterGapHours, tpLevels, slLevels, totalCells]);

  const appliedShift = Math.max(0, Math.round(parseNumberInput(timeShiftInput) ?? 0));
  const appliedPriceChange = enablePriceChange && considerPriceChange ? (parseNumberInput(priceChangeInput) ?? 0) : null;

  return <Card>
    <CardContent className="grid gap-4">
      <div className="grid gap-1">
        <h2 className="text-lg font-semibold">Среднее матожидание сетапов</h2>
        <p className="text-xs text-muted-foreground">{description}</p>
      </div>
      <div className="flex flex-wrap items-end gap-3">
        <TimeRangeControls setId={setId} setBounds={setBounds} value={timeRange} onChange={setTimeRange} />
        <Field label="Time-shift, мин" className="w-32">
          {(control) => <Input {...control} inputMode="numeric" value={timeShiftInput} onChange={(event) => setTimeShiftInput(event.target.value)} />}
        </Field>
        {enablePriceChange && (
          <>
            <label className="flex items-center gap-2 text-sm">
              <Checkbox checked={considerPriceChange} onCheckedChange={(checked) => setConsiderPriceChange(checked === true)} />
              Учитывать смещение цены
            </label>
            <Field label="Смещение цены, %" className="w-36">
              {(control) => <Input {...control} inputMode="decimal" disabled={!considerPriceChange} value={priceChangeInput} onChange={(event) => setPriceChangeInput(event.target.value)} />}
            </Field>
          </>
        )}
        {running
          ? <Button variant="outline" onClick={stop}>Остановить</Button>
          : <Button disabled={setId === ""} onClick={() => { void run(appliedShift, appliedPriceChange); }}>Рассчитать</Button>}
        {progress.total > 0 && <Badge variant={progress.done >= progress.total ? "success" : "info"}>Ячеек: {progress.done} / {progress.total}</Badge>}
      </div>
      <div className="flex flex-wrap items-center gap-6 text-sm">
        <label className="flex items-center gap-2">
          <span className="text-muted-foreground">Красный при ≤</span>
          <input
            type="range"
            min={-1}
            max={1}
            step={0.05}
            aria-label="Красный при"
            className="h-2 w-36 cursor-pointer appearance-none rounded-lg bg-muted"
            value={gradientMinDraft}
            onChange={(event) => setGradientMinDraft(Number(event.target.value))}
            onPointerUp={() => setGradientMin(gradientMinDraft)}
            onKeyUp={() => setGradientMin(gradientMinDraft)}
            onBlur={() => setGradientMin(gradientMinDraft)}
          />
          <span className="tabular-nums">{gradientMinDraft.toFixed(2)}</span>
        </label>
        <label className="flex items-center gap-2">
          <span className="text-muted-foreground">Зелёный при ≥</span>
          <input
            type="range"
            min={-1}
            max={1}
            step={0.05}
            aria-label="Зелёный при"
            className="h-2 w-36 cursor-pointer appearance-none rounded-lg bg-muted"
            value={gradientMaxDraft}
            onChange={(event) => setGradientMaxDraft(Number(event.target.value))}
            onPointerUp={() => setGradientMax(gradientMaxDraft)}
            onKeyUp={() => setGradientMax(gradientMaxDraft)}
            onBlur={() => setGradientMax(gradientMaxDraft)}
          />
          <span className="tabular-nums">{gradientMaxDraft.toFixed(2)}</span>
        </label>
      </div>
      {error && <Alert variant="destructive" role="alert"><AlertDescription>{error}</AlertDescription></Alert>}
      {metrics !== null && (
        <div className="flex flex-wrap gap-2 text-xs">
          <Badge>{clusterize ? "После кластеризации" : "Сетапов"}: {metrics.totalMatchingSetups}</Badge>
          {metrics.avgSelectionOrdinal !== null && <Badge>Средний номер: {metrics.avgSelectionOrdinal.toFixed(2)}</Badge>}
          {metrics.avgSelectionDelayMinutes !== null && <Badge>Средняя задержка: {formatDuration(metrics.avgSelectionDelayMinutes)}</Badge>}
        </div>
      )}
      <div className="overflow-x-auto">
        <Table className="table-fixed">
          <TableHeader>
            <TableRow>
              <TableHead className="w-16">TP \ SL</TableHead>
              {slLevels.map((sl) => <TableHead key={sl} numeric>{sl}%</TableHead>)}
            </TableRow>
          </TableHeader>
          <TableBody>
            {tpLevels.map((tp) => (
              <TableRow key={tp}>
                <TableHead numeric>{tp}%</TableHead>
                {slLevels.map((sl) => {
                  const cell = cells.get(`${tp}:${sl}`);
                  if (cell === undefined) {
                    if (running && pendingCell === `${tp}:${sl}`) {
                      return <TableCell key={sl} numeric className="text-muted-foreground"><Spinner label="Расчёт ячейки" /></TableCell>;
                    }
                    return <TableCell key={sl} numeric className="text-muted-foreground/60">{running ? "…" : "·"}</TableCell>;
                  }
                  if (cell.riskExpectancy === null) {
                    return <TableCell key={sl} numeric className="text-muted-foreground" title="avg —% · R —">—</TableCell>;
                  }
                  return <TableCell
                    key={sl}
                    numeric
                    className={`tabular-nums font-bold analytics-cell-text-${matrixCellTextStep(cell.riskExpectancy, gradientMin, gradientMax)}`}
                    title={`avg ${cell.avgMovePct?.toFixed(2) ?? "—"}% · R ${cell.riskExpectancy.toFixed(3)}`}
                  >
                    {cell.riskExpectancy.toFixed(2)}
                  </TableCell>;
                })}
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </div>
      <p className="text-xs text-muted-foreground">Ячейка — матожидание в единицах риска: среднее движение по закрытым сделкам / SL. «—» — нет закрытых сделок. Спорная свеча считается Stop Loss. Цвет жирных значений интерполируется от красного к зелёному между порогами ползунков.</p>
    </CardContent>
  </Card>;
}

function MonthOptions({ setId }: { setId: string }) {
  const [months, setMonths] = useState<string[]>([]);
  useEffect(() => {
    let cancelled = false;
    void fetchAnalyticsMonths(setId)
      .then((items) => { if (!cancelled) setMonths(items); })
      .catch(() => undefined);
    return () => { cancelled = true; };
  }, [setId]);
  return <>{months.map((month) => <option key={month} value={month}>{month}</option>)}</>;
}

function utcDayOf(unix: number): string {
  return new Date(unix * 1000).toISOString().slice(0, 10);
}

function monthDayBounds(month: string): { from: string; to: string } {
  const [year, monthNumber] = month.split("-").map(Number);
  const lastDay = new Date(Date.UTC(year ?? 0, monthNumber ?? 0, 0)).toISOString().slice(0, 10);
  return { from: `${month}-01`, to: lastDay };
}

/** Time filter state shared by every section: month selector synced with a date pair. */
interface TimeRangeValue {
  mode: "all" | "month" | "custom";
  month: string;
  fromDay: string | null;
  toDay: string | null;
}

function timeRangeParams(value: TimeRangeValue): { month?: string; fromDay?: string; toDay?: string } {
  if (value.mode === "month") return { month: value.month };
  if (value.mode === "custom" && value.fromDay !== null && value.toDay !== null && value.fromDay !== "" && value.toDay !== "") {
    return { fromDay: value.fromDay, toDay: value.toDay };
  }
  return {};
}

function timeRangeBounds(value: TimeRangeValue, setBounds: { from: string; to: string } | null): { from: string; to: string } | null {
  if (value.mode === "custom") {
    return value.fromDay !== null && value.toDay !== null && value.fromDay !== "" && value.toDay !== "" ? { from: value.fromDay, to: value.toDay } : null;
  }
  if (value.mode === "month") return monthDayBounds(value.month);
  return setBounds;
}

/** Month selector + date pair: month choice auto-fills the dates, manual dates set the dash. */
function TimeRangeControls({ setId, setBounds, value, onChange }: {
  setId: string;
  setBounds: { from: string; to: string } | null;
  value: TimeRangeValue;
  onChange: (next: TimeRangeValue) => void;
}) {
  const auto = value.mode === "month" ? monthDayBounds(value.month) : value.mode === "all" ? setBounds : null;
  const fromValue = value.mode === "custom" ? (value.fromDay ?? "") : (auto?.from ?? "");
  const toValue = value.mode === "custom" ? (value.toDay ?? "") : (auto?.to ?? "");
  return <>
    <Field label="Период" className="w-44">
      {(control) => <Select
        {...control}
        value={value.mode === "custom" ? "custom" : value.month}
        onChange={(event) => {
          const next = event.target.value;
          if (next === "all") onChange({ mode: "all", month: "all", fromDay: null, toDay: null });
          else if (next !== "custom") onChange({ mode: "month", month: next, fromDay: null, toDay: null });
        }}
      >
        <option value="all">Всё время</option>
        <MonthOptions setId={setId} />
        <option value="custom" disabled>—</option>
      </Select>}
    </Field>
    <Field label="С даты" className="w-40">
      {(control) => <Input {...control} type="date" value={fromValue} onChange={(event) => onChange({ mode: "custom", month: "custom", fromDay: event.target.value, toDay: toValue })} />}
    </Field>
    <Field label="По дату" className="w-40">
      {(control) => <Input {...control} type="date" value={toValue} onChange={(event) => onChange({ mode: "custom", month: "custom", fromDay: fromValue, toDay: event.target.value })} />}
    </Field>
  </>;
}

interface TradeRowWithSource extends TradeRow {
  fromCache: boolean;
}

interface DailyExpectancyPoint {
  day: string;
  riskExpectancy: number | null;
  closedCount: number;
  tpCount: number;
  slCount: number;
  computedCount: number;
  cachedCount: number;
}

/** Groups closed trades into local-day points, like the reference project. */
function buildDailyExpectancy(rows: TradeRowWithSource[]): DailyExpectancyPoint[] {
  const buckets = new Map<string, { values: number[]; closedCount: number; tpCount: number; slCount: number; computedCount: number; cachedCount: number }>();
  for (const row of rows) {
    const day = new Date(row.setup.closeTimeUnix * 1000).toLocaleDateString("en-CA");
    const bucket = buckets.get(day) ?? { values: [], closedCount: 0, tpCount: 0, slCount: 0, computedCount: 0, cachedCount: 0 };
    bucket.computedCount += 1;
    if (row.fromCache) bucket.cachedCount += 1;
    if (row.riskReturn !== null) {
      bucket.values.push(row.riskReturn);
      bucket.closedCount += 1;
      if (row.status === "tp") bucket.tpCount += 1;
      else if (row.status === "sl") bucket.slCount += 1;
    }
    buckets.set(day, bucket);
  }
  return Array.from(buckets.entries())
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([day, bucket]) => ({
      day,
      riskExpectancy: bucket.values.length > 0 ? bucket.values.reduce((a, b) => a + b, 0) / bucket.values.length : null,
      closedCount: bucket.closedCount,
      tpCount: bucket.tpCount,
      slCount: bucket.slCount,
      computedCount: bucket.computedCount,
      cachedCount: bucket.cachedCount,
    }));
}

function dayLabel(day: string): string {
  const [, month, dayNumber] = day.split("-");
  return `${dayNumber}.${month}`;
}

/** Daily risk-expectancy line chart with a regression trend, ported from the reference project. */
function DailyExpectancyChart({ items }: { items: DailyExpectancyPoint[] }) {
  if (items.length === 0) {
    return <p className="text-xs text-muted-foreground">График появится после расчёта первых строк.</p>;
  }
  const values = items.map((item) => item.riskExpectancy).filter((value): value is number => value !== null && Number.isFinite(value));
  if (values.length === 0) {
    return <p className="text-xs text-muted-foreground">Пока нет дней с закрытыми TP/SL сделками.</p>;
  }
  const width = 760;
  const height = 280;
  const left = 64;
  const right = 24;
  const top = 24;
  const bottom = 56;
  const plotWidth = width - left - right;
  const plotHeight = height - top - bottom;
  const minValue = Math.min(0, ...values);
  const maxValue = Math.max(0, ...values);
  const padding = Math.max(0.1, (maxValue - minValue) * 0.08);
  const yMin = minValue === maxValue ? minValue - 0.5 : minValue - padding;
  const yMax = minValue === maxValue ? maxValue + 0.5 : maxValue + padding;
  const yRange = yMax - yMin;
  const xForIndex = (index: number) => left + (items.length === 1 ? plotWidth / 2 : (index / (items.length - 1)) * plotWidth);
  const yForValue = (value: number) => top + ((yMax - value) / yRange) * plotHeight;
  const zeroY = yMin <= 0 && yMax >= 0 ? yForValue(0) : null;
  const labelStep = Math.max(1, Math.ceil(items.length / 8));
  const segments: string[][] = [];
  let current: string[] = [];
  for (let index = 0; index < items.length; index += 1) {
    const item = items[index]!;
    if (item.riskExpectancy === null || !Number.isFinite(item.riskExpectancy)) {
      if (current.length > 0) { segments.push(current); current = []; }
      continue;
    }
    current.push(`${xForIndex(index)},${yForValue(item.riskExpectancy)}`);
  }
  if (current.length > 0) segments.push(current);

  const trendPoints = items
    .map((item, index) => ({ index, value: item.riskExpectancy }))
    .filter((point): point is { index: number; value: number } => point.value !== null && Number.isFinite(point.value));
  let trendLine: { x1: number; y1: number; x2: number; y2: number; slope: number; totalChange: number } | null = null;
  if (trendPoints.length > 1) {
    const count = trendPoints.length;
    const meanX = trendPoints.reduce((sum, point) => sum + point.index, 0) / count;
    const meanY = trendPoints.reduce((sum, point) => sum + point.value, 0) / count;
    let covariance = 0;
    let variance = 0;
    for (const point of trendPoints) {
      covariance += (point.index - meanX) * (point.value - meanY);
      variance += (point.index - meanX) ** 2;
    }
    if (variance > 0) {
      const slope = covariance / variance;
      const trendYAt = (index: number) => meanY + slope * (index - meanX);
      const first = trendPoints[0]!;
      const last = trendPoints[count - 1]!;
      trendLine = {
        x1: xForIndex(first.index),
        y1: yForValue(trendYAt(first.index)),
        x2: xForIndex(last.index),
        y2: yForValue(trendYAt(last.index)),
        slope,
        totalChange: trendYAt(last.index) - trendYAt(first.index),
      };
    }
  }

  return (
    <svg className="w-full max-w-3xl" viewBox={`0 0 ${width} ${height}`} role="img" aria-label="Дневное матожидание в рисках">
      <line className="stroke-muted-foreground/40" x1={left} y1={top} x2={left} y2={height - bottom} />
      <line className="stroke-muted-foreground/40" x1={left} y1={height - bottom} x2={width - right} y2={height - bottom} />
      {zeroY !== null ? <line className="stroke-muted-foreground/50" x1={left} y1={zeroY} x2={width - right} y2={zeroY} /> : null}
      {[yMin, (yMin + yMax) / 2, yMax].map((value) => (
        <g key={value.toFixed(4)}>
          <line className="stroke-muted-foreground/20" x1={left} y1={yForValue(value)} x2={width - right} y2={yForValue(value)} />
          <text className="fill-muted-foreground text-[10px]" x={left - 10} y={yForValue(value) + 4} textAnchor="end">{formatRisk(value)}</text>
        </g>
      ))}
      {segments.map((segment, index) =>
        segment.length > 1 ? <polyline key={index} className="fill-none stroke-primary stroke-2" points={segment.join(" ")} /> : null,
      )}
      {trendLine !== null ? (
        <line
          className="stroke-warning"
          strokeWidth={2}
          strokeDasharray="4 4"
          x1={trendLine.x1} y1={trendLine.y1} x2={trendLine.x2} y2={trendLine.y2}
        >
          <title>{`Тренд дневного матожидания: ${formatRisk(trendLine.slope)}/день, за период ${formatRisk(trendLine.totalChange)}`}</title>
        </line>
      ) : null}
      {items.map((item, index) => {
        const x = xForIndex(index);
        const showLabel = index % labelStep === 0 || index === items.length - 1;
        return (
          <g key={item.day}>
            {item.riskExpectancy !== null && Number.isFinite(item.riskExpectancy) ? (
              <circle
                className={item.riskExpectancy >= 0 ? "fill-success" : "fill-danger"}
                cx={x} cy={yForValue(item.riskExpectancy)} r={4}
              >
                <title>
                  {`${dayLabel(item.day)}: ${formatRisk(item.riskExpectancy)}, закрыто ${item.closedCount}, TP ${item.tpCount}, SL ${item.slCount}, из кеша ${item.cachedCount}/${item.computedCount}`}
                </title>
              </circle>
            ) : null}
            {showLabel ? <text className="fill-muted-foreground text-[10px]" x={x} y={height - 26} textAnchor="middle">{dayLabel(item.day)}</text> : null}
          </g>
        );
      })}
    </svg>
  );
}

interface TradesSectionProps {
  setId: string;
  description: string;
  direction: "short" | "long";
  enablePriceChange: boolean;
  clusterize: boolean;
  maturity: number;
  clusterGapHours: number | null;
  setBounds: { from: string; to: string } | null;
}

function TradesSection({ setId, description, direction, enablePriceChange, clusterize, maturity, clusterGapHours, setBounds }: TradesSectionProps) {
  const [tpInput, setTpInput] = useState("4");
  const [slInput, setSlInput] = useState("4");
  const [timeShiftInput, setTimeShiftInput] = useState("0");
  const [timeRange, setTimeRange] = useState<TimeRangeValue>({ mode: "all", month: "all", fromDay: null, toDay: null });
  const [considerPriceChange, setConsiderPriceChange] = useState(false);
  const [priceChangeInput, setPriceChangeInput] = useState("0");
  const [rows, setRows] = useState<TradeRowWithSource[]>([]);
  const [filteredCount, setFilteredCount] = useState(0);
  const [running, setRunning] = useState(false);
  const [progress, setProgress] = useState({ done: 0, total: 0 });
  const [error, setError] = useState<string | null>(null);
  const runIdRef = useRef(0);

  const params = useMemo(() => ({
    set: setId,
    direction,
    tpPct: parseNumberInput(tpInput) ?? 4,
    slPct: parseNumberInput(slInput) ?? 4,
    ...(Math.max(0, Math.round(parseNumberInput(timeShiftInput) ?? 0)) > 0 ? { timeShiftMinutes: Math.max(0, Math.round(parseNumberInput(timeShiftInput) ?? 0)) } : {}),
    ...timeRangeParams(timeRange),
    ...(clusterize ? {
      setupMaturity: maturity,
      ...(clusterGapHours !== null && clusterGapHours > 0 ? { clusterGapHours } : {}),
    } : {}),
    ...(enablePriceChange && considerPriceChange ? { priceChangePct: parseNumberInput(priceChangeInput) ?? 0 } : {}),
  }), [setId, direction, tpInput, slInput, timeShiftInput, timeRange, clusterize, maturity, clusterGapHours, enablePriceChange, considerPriceChange, priceChangeInput]);

  const run = useCallback(async () => {
    const runId = runIdRef.current + 1;
    runIdRef.current = runId;
    setRunning(true);
    setError(null);
    setRows([]);
    setFilteredCount(0);
    const stale = () => runIdRef.current !== runId;
    try {
      const selection = await fetchAnalyticsSetups(params);
      if (stale()) return;
      const setups = selection.setups;
      setProgress({ done: 0, total: setups.length });
      const bulk = await fetchTradeRows(params);
      if (stale()) return;
      const cachedByKey = new Map(bulk.items.map((row) => [`${row.setup.symbol}:${row.setup.closeTimeUnix}`, row]));
      const collected: TradeRowWithSource[] = [];
      let filtered = 0;
      let done = 0;
      const flush = () => {
        setRows([...collected]);
        setFilteredCount(filtered);
        setProgress({ done, total: setups.length });
      };
      for (const setup of setups) {
        const key = `${setup.symbol}:${setup.closeTimeUnix}`;
        const cachedRow = cachedByKey.get(key);
        let row: TradeRow;
        let fromCache = true;
        if (cachedRow === undefined) {
          row = await fetchTradeRow(setup, params);
          if (stale()) return;
          fromCache = false;
        } else {
          row = cachedRow;
        }
        done += 1;
        if (row.status === "price_change_filtered") {
          filtered += 1;
        } else {
          collected.push({ ...row, fromCache });
        }
        flush();
      }
    } catch (cause) {
      if (!stale()) setError(cause instanceof Error ? cause.message : "ошибка расчёта");
    } finally {
      if (!stale()) setRunning(false);
    }
  }, [params]);

  const effectiveDays = timeRangeBounds(timeRange, setBounds);
  const visibleRows = useMemo(() => {
    if (effectiveDays === null) return rows;
    const from = Math.floor(new Date(`${effectiveDays.from}T00:00:00Z`).getTime() / 1000);
    const to = Math.floor(new Date(`${effectiveDays.to}T23:59:59Z`).getTime() / 1000);
    return rows.filter((row) => row.setup.closeTimeUnix >= from && row.setup.closeTimeUnix <= to);
  }, [rows, effectiveDays]);

  const summary = useMemo(() => {
    const closed = visibleRows.filter((row) => row.status === "tp" || row.status === "sl");
    const tp = closed.filter((row) => row.status === "tp");
    const risks = closed.map((row) => row.riskReturn).filter((value): value is number => value !== null);
    const avgRisk = risks.length > 0 ? risks.reduce((a, b) => a + b, 0) / risks.length : null;
    return { closed: closed.length, winRate: closed.length > 0 ? (tp.length / closed.length) * 100 : null, avgRisk };
  }, [visibleRows]);

  const dailyPoints = useMemo(() => buildDailyExpectancy(visibleRows), [visibleRows]);

  return <Card>
    <CardContent className="grid gap-4">
      <div className="grid gap-1">
        <h2 className="text-lg font-semibold">Матожидание по сетапам</h2>
        <p className="text-xs text-muted-foreground">{description}</p>
      </div>
      <div className="flex flex-wrap items-end gap-3">
        <Field label="TP, %" className="w-28">
          {(control) => <Input {...control} inputMode="decimal" value={tpInput} onChange={(event) => setTpInput(event.target.value)} />}
        </Field>
        <Field label="SL, %" className="w-28">
          {(control) => <Input {...control} inputMode="decimal" value={slInput} onChange={(event) => setSlInput(event.target.value)} />}
        </Field>
        <Field label="Time-shift, мин" className="w-32">
          {(control) => <Input {...control} inputMode="numeric" value={timeShiftInput} onChange={(event) => setTimeShiftInput(event.target.value)} />}
        </Field>
        <TimeRangeControls setId={setId} setBounds={setBounds} value={timeRange} onChange={setTimeRange} />
        {enablePriceChange && (
          <>
            <label className="flex items-center gap-2 text-sm">
              <Checkbox checked={considerPriceChange} onCheckedChange={(checked) => setConsiderPriceChange(checked === true)} />
              Учитывать смещение цены
            </label>
            <Field label="Смещение цены, %" className="w-36">
              {(control) => <Input {...control} inputMode="decimal" disabled={!considerPriceChange} value={priceChangeInput} onChange={(event) => setPriceChangeInput(event.target.value)} />}
            </Field>
          </>
        )}
        {running
          ? <Button variant="outline" onClick={() => { runIdRef.current += 1; setRunning(false); }}>Остановить</Button>
          : <Button disabled={setId === ""} onClick={() => { void run(); }}>Рассчитать</Button>}
        {progress.total > 0 && <Badge variant={progress.done >= progress.total ? "success" : "info"}>Сделок: {progress.done} / {progress.total}</Badge>}
      </div>
      {error && <Alert variant="destructive" role="alert"><AlertDescription>{error}</AlertDescription></Alert>}
      <div className="flex flex-wrap gap-2 text-xs">
        {summary.winRate !== null && <Badge variant={summary.avgRisk !== null && summary.avgRisk >= 0 ? "success" : "danger"}>Win rate: {summary.winRate.toFixed(1)}%</Badge>}
        {summary.avgRisk !== null && <Badge>Матожидание: {formatRisk(summary.avgRisk)} по {summary.closed} закрытым</Badge>}
        {filteredCount > 0 && <Badge variant="warning">Отфильтровано смещением цены: {filteredCount}</Badge>}
      </div>
      {rows.length > 0 && <DailyExpectancyChart items={dailyPoints} />}
      {rows.length === 0 && !running && !error && <EmptyState title="Нет рассчитанных сделок" description="Выберите параметры и нажмите «Рассчитать». Сначала загружаются кешированные строки, затем досчитываются остальные." />}
      {rows.length > 0 && (
        <div className="overflow-x-auto">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Инструмент</TableHead>
                <TableHead>Время сетапа</TableHead>
                <TableHead numeric>Вход</TableHead>
                <TableHead numeric className="text-success">TP</TableHead>
                <TableHead numeric className="text-danger">SL</TableHead>
                <TableHead>Закрытие</TableHead>
                <TableHead numeric>Результат</TableHead>
                <TableHead>Статус</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {visibleRows.map((row) => (
                <TableRow key={`${row.setup.symbol}:${row.setup.closeTimeUnix}`}>
                  <TableCell className="font-medium">{row.setup.symbol}</TableCell>
                  <TableCell>{formatTime(row.setup.closeTimeUnix)}</TableCell>
                  <TableCell numeric>{formatPrice(row.entryPrice)}</TableCell>
                  <TableCell numeric className="text-success">{formatPrice(row.tpPrice)}</TableCell>
                  <TableCell numeric className="text-danger">{formatPrice(row.slPrice)}</TableCell>
                  <TableCell>{formatTime(row.closeTimeUnix)}</TableCell>
                  <TableCell numeric className={(row.riskReturn ?? 0) >= 0 ? "text-success" : "text-danger"}>{formatRisk(row.riskReturn)}</TableCell>
                  <TableCell>
                    <Badge variant={row.status === "tp" ? "success" : row.status === "sl" ? "danger" : "neutral"}>
                      {row.status === "tp" ? "TP" : row.status === "sl" ? "SL" : row.status === "unclosed" ? "не закрылась" : "нет данных"}
                    </Badge>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      )}
    </CardContent>
  </Card>;
}

function AnalyticsCacheCard() {
  const [summary, setSummary] = useState<{ tables: { name: string; count: number }[]; total: number } | null>(null);
  const [state, setState] = useState<"loading" | "ready" | "error">("loading");
  const [message, setMessage] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [reloadToken, setReloadToken] = useState(0);

  useEffect(() => {
    let cancelled = false;
    void fetchAnalyticsCacheSummary()
      .then((result) => {
        if (cancelled) return;
        setSummary(result);
        setState("ready");
      })
      .catch(() => {
        if (!cancelled) setState("error");
      });
    return () => { cancelled = true; };
  }, [reloadToken]);

  const handleClear = async () => {
    setBusy(true);
    setMessage(null);
    try {
      const result = await clearAnalyticsCache();
      setMessage(`Удалено записей: ${result.total}`);
      setReloadToken((token) => token + 1);
    } catch {
      setMessage("Не удалось очистить кеш");
    } finally {
      setBusy(false);
    }
  };

  return <Card>
    <CardContent className="grid gap-4">
      <div className="grid gap-1">
        <h2 className="text-lg font-semibold">Кеш аналитики</h2>
        <p className="text-xs text-muted-foreground">Кеш результатов хранится в БД платформы рядом с сетапами. Догрузка истории не сбрасывает его автоматически; очистка удаляет только кеш аналитики, не трогая запуски, сетапы и рыночные данные.</p>
      </div>
      {state === "loading" && <Spinner label="Загрузка сводки…" />}
      {state === "error" && <Alert variant="destructive" role="alert">Сводка кеша недоступна</Alert>}
      {message && <Alert variant="success" role="status"><AlertDescription>{message}</AlertDescription></Alert>}
      {state === "ready" && summary !== null && (
        <div className="grid gap-3">
          <div className="overflow-x-auto">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Таблица кеша</TableHead>
                  <TableHead numeric>Записей</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {summary.tables.map((table) => (
                  <TableRow key={table.name}>
                    <TableCell>{table.name}</TableCell>
                    <TableCell numeric>{table.count}</TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <Button variant="outline" disabled={busy} onClick={() => { setState("loading"); setReloadToken((token) => token + 1); }}>Обновить</Button>
            <Button variant="outline" disabled={busy || summary.total === 0} onClick={() => { void handleClear(); }}>Очистить кеш</Button>
            <Badge>Всего: {summary.total}</Badge>
          </div>
        </div>
      )}
    </CardContent>
  </Card>;
}

type AnalyticsSection = "short" | "long" | "cache";

export function AnalyticsPage() {
  const [sets, setSets] = useState<AnalyticsSet[]>([]);
  const [setsState, setSetsState] = useState<"loading" | "ready" | "error">("loading");
  const [setId, setSetId] = useState("");
  const [section, setSection] = useState<AnalyticsSection>("short");
  const [clusterize, setClusterize] = useState(true);
  const [clusterGapInput, setClusterGapInput] = useState("8");
  const [maturityDraft, setMaturityDraft] = useState(0.5);
  const [maturity, setMaturity] = useState(0.5);

  useEffect(() => {
    let cancelled = false;
    void fetchAnalyticsSets()
      .then((items) => {
        if (cancelled) return;
        setSets(items);
        setSetsState("ready");
        setSetId((current) => (current === "" && items.length > 0 ? (items[0]!.setId) : current));
      })
      .catch(() => {
        if (!cancelled) setSetsState("error");
      });
    return () => { cancelled = true; };
  }, []);

  const appliedClusterGapHours = parseNumberInput(clusterGapInput);
  const activeSet = sets.find((set) => set.setId === setId);
  const setBounds = activeSet?.fromUnix != null && activeSet?.toUnix != null
    ? { from: utcDayOf(activeSet.fromUnix), to: utcDayOf(activeSet.toUnix) }
    : null;

  return <section aria-labelledby="analytics-heading" className="grid gap-4">
    <PageHeader id="analytics-heading" title="Аналитика" description="Backtest наборов сетапов из завершённых исторических запусков: матожидание TP/SL, зрелость кластеров, построчные закрытия. Расчёты читают минутные свечи через сервис данных; результаты кешируются." />
    <Card>
      <CardContent className="grid gap-3">
        <div className="flex flex-wrap items-end gap-3">
          <Field label="Набор сетапов" className="w-full max-w-md">
            {(control) => (
              <Select {...control} value={setId} disabled={setsState !== "ready"} onChange={(event) => setSetId(event.target.value)}>
                {sets.length === 0 && <option value="">Нет завершённых запусков</option>}
                {sets.map((set) => <option key={set.setId} value={set.setId}>{set.label}</option>)}
              </Select>
            )}
          </Field>
          {setsState === "ready" && activeSet !== undefined && (
            <div className="grid gap-2">
              <div className="flex flex-wrap gap-2 text-xs">
                <Badge>Сетапов: {activeSet.setupCount}</Badge>
                <Badge>{activeSet.exchange} · {activeSet.category}{activeSet.symbol === null ? "" : ` · ${activeSet.symbol}`}</Badge>
              </div>
              <details>
                <summary className="cursor-pointer font-medium">Снимок параметров запуска</summary>
                <dl className="mt-2 grid gap-2 text-sm sm:grid-cols-2">
                  {Object.entries(activeSet.params).map(([key, value]) => (
                    <div key={key} className="break-words">
                      <dt className="text-muted-foreground">{parameterLabels[key] ?? key}</dt>
                      <dd>{value}</dd>
                    </div>
                  ))}
                </dl>
              </details>
            </div>
          )}
        </div>
        <div className="flex flex-wrap items-end gap-3">
          <label className="flex items-center gap-2 text-sm">
            <Checkbox checked={clusterize} onCheckedChange={(checked) => setClusterize(checked === true)} />
            Кластеризовать сетапы
          </label>
          <Field label={`Зрелость сетапа в кластере: ${maturityDraft.toFixed(1)}`} className="w-64">
            {(control) => <Input
              {...control}
              type="range"
              min={0}
              max={1}
              step={0.1}
              disabled={!clusterize}
              value={maturityDraft}
              onChange={(event) => setMaturityDraft(Number(event.target.value))}
              onPointerUp={() => setMaturity(maturityDraft)}
              onKeyUp={() => setMaturity(maturityDraft)}
              onBlur={() => setMaturity(maturityDraft)}
            />}
          </Field>
          <Field label="Разрыв кластера, ч" className="w-32">
            {(control) => <Input {...control} inputMode="decimal" disabled={!clusterize} value={clusterGapInput} onChange={(event) => setClusterGapInput(event.target.value)} />}
          </Field>
        </div>
        <p className="text-xs text-muted-foreground">Настройки кластеризации действуют одновременно на все разделы SHORT и LONG. 0 — первый сетап кластера, 1 — последний, 0.5 — середина; соседние сигналы одной монеты с разрывом до {(appliedClusterGapHours ?? 8).toString()} часов образуют кластер. При выключенной галочке кластеризация не выполняется — в расчёт входят все сетапы.</p>
      </CardContent>
    </Card>
    <SegmentedList role="tablist" aria-label="Подразделы аналитики">
      <SegmentedTrigger role="tab" aria-selected={section === "short"} onClick={() => setSection("short")}>SHORT</SegmentedTrigger>
      <SegmentedTrigger role="tab" aria-selected={section === "long"} onClick={() => setSection("long")}>LONG</SegmentedTrigger>
      <SegmentedTrigger role="tab" aria-selected={section === "cache"} onClick={() => setSection("cache")}>Кеш</SegmentedTrigger>
    </SegmentedList>
    {setsState === "loading" && <Spinner label="Загрузка наборов…" />}
    {setsState === "error" && <Alert variant="destructive" role="alert">Наборы недоступны: не удалось получить список исторических запусков</Alert>}
    {setsState === "ready" && sets.length === 0 && (
      <EmptyState title="Нет наборов для аналитики" description="Аналитика работает по завершённым историческим запускам. Создайте запуск на странице «Запуски» и дождитесь его завершения." />
    )}
    <div hidden={section !== "short"} className="grid gap-4">
      <MatrixSection
        setId={setId}
        description="SHORT: TP×SL по сетапам, выбранным из кластеров по зрелости. Период ограничивает исходные сигналы до кластеризации; горизонт сделки концом периода не обрезается."
        direction="short"
        tpLevels={SHORT_MATRIX_LEVELS}
        slLevels={SHORT_MATRIX_LEVELS}
        enablePriceChange
        gradientMinDefault={0.05}
        gradientMaxDefault={0.2}
        clusterize={clusterize}
        maturity={maturity}
        clusterGapHours={appliedClusterGapHours}
        setBounds={setBounds}
      />
      <TradesSection
        setId={setId}
        description="SHORT: построчные сделки — вход по close свечи через time-shift, закрытие по TP или SL за 7 дней. Сначала показываются кешированные строки, недостающие досчитываются."
        direction="short"
        enablePriceChange
        clusterize={clusterize}
        maturity={maturity}
        clusterGapHours={appliedClusterGapHours}
        setBounds={setBounds}
      />
    </div>
    <div hidden={section !== "long"} className="grid gap-4">
      <MatrixSection
        setId={setId}
        description="LONG: TP×SL по сетапам набора с общими настройками кластеризации и периода. Гейт смещения цены зеркален: LONG заходит только при падении цены на заданный процент."
        direction="long"
        tpLevels={LONG_TP_LEVELS}
        slLevels={LONG_SL_LEVELS}
        enablePriceChange
        gradientMinDefault={0.05}
        gradientMaxDefault={0.5}
        clusterize={clusterize}
        maturity={maturity}
        clusterGapHours={appliedClusterGapHours}
        setBounds={setBounds}
      />
      <TradesSection
        setId={setId}
        description="LONG: построчные закрытия по уровням TP/SL с ценой входа, временем касания и результатом в рисках."
        direction="long"
        enablePriceChange={false}
        clusterize={clusterize}
        maturity={maturity}
        clusterGapHours={appliedClusterGapHours}
        setBounds={setBounds}
      />
    </div>
    <div hidden={section !== "cache"} className="grid gap-4">
      <AnalyticsCacheCard />
    </div>
  </section>;
}
