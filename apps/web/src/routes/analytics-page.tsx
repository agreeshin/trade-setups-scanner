import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import { AnalyticsTrendControls } from "../components/analytics-trend-controls";
import { AnalyticsForecastChart, DailyCalendarGrid, DailyObservedControls } from "../components/analytics-forecast-chart";
import { Alert, AlertDescription } from "../components/ui/alert";
import { Badge } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import { Checkbox } from "../components/ui/checkbox";
import { EmptyState } from "../components/ui/empty-state";
import { Field } from "../components/ui/field";
import { Input } from "../components/ui/input";
import { PageHeader } from "../components/ui/page-header";
import { PageSidebar, PageSidebarPanel } from "../components/ui/page-sidebar";
import { InfoNote } from "../components/ui/info-note";
import { Select } from "../components/ui/select";
import { Separator } from "../components/ui/separator";
import { Spinner } from "../components/ui/spinner";
import { parameterLabels } from "../lib/parameter-labels";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "../components/ui/table";
import {
  clearAnalyticsCache,
  fetchAnalyticsCacheSummary,
  fetchAnalyticsMonths,
  fetchAnalyticsSets,
  fetchMatrixGrid,
  fetchAnalyticsSetups,
  fetchTradeRow,
  fetchTradeRows,
  fetchDailyTrend,
  type TrendBendDays,
  type TrendMode,
  type TrendProjectionRequest,
  type TrendProjectionResponse,
  type AnalyticsSet,
  type MatrixCellRef,
  type SelectionMetrics,
  type TradeRow,
} from "../lib/analytics-api";
import {
  type AnalyticsSectionKey,
  type AnalyticsUserSettings,
} from "../lib/analytics-settings";
import { updateSharedAnalyticsSettings, useSharedAnalyticsSettings } from "../lib/analytics-settings-store";
import { effectiveTimezone, formatUnix, interfaceSettings, subscribeInterfaceSettings } from "../lib/datetime-format";
import { buildDailyExpectancy, buildDailyExpectancyModel, observedDailyPoints, type ObservedDailyMode, type DailyExpectancyPoint } from "../lib/daily-expectancy";
import { buildForecastRecipe, forecastRecipeLabel, type ForecastRecipeDraft } from "../lib/forecast-recipe";
import { buildObservedForecastDays } from "../lib/forecast-observations";
import type { ForecastCurveRequest } from "../lib/forecast-api";
import { isTrendBlendLambda } from "../lib/optimization-api";

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
  return formatUnix(unix);
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
  scope: AnalyticsSelectionScope;
  onScopeChange: (next: AnalyticsSelectionScope) => void;
  trendBendDays: TrendBendDays;
  trendMode: TrendMode;
  forecast: ForecastRecipeDraft | undefined;
  trendBlendLambda: string;
  observationCutoff: number;
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
  initial?: Record<string, unknown> | undefined;
  reportState?: ((snapshot: Record<string, unknown>) => void) | undefined;
  onCommit?: (() => void) | undefined;
  runToken: number;
}

function MatrixSection({
  setId, description, direction, tpLevels, slLevels, enablePriceChange,
  gradientMinDefault, gradientMaxDefault, clusterize, maturity, clusterGapHours, setBounds,
  initial, reportState, onCommit, runToken, trendBendDays, trendMode, forecast, trendBlendLambda, observationCutoff, scope, onScopeChange,
}: MatrixSectionProps) {
  const { timeRange, timeShiftInput, considerPriceChange, priceChangeInput } = scope;
  const setTimeRange = (value: TimeRangeValue) => onScopeChange({ ...scope, timeRange: value });
  const [gradientMinDraft, setGradientMinDraft] = useState(() => savedNumber(initial?.gradientMin, gradientMinDefault));
  const [gradientMaxDraft, setGradientMaxDraft] = useState(() => savedNumber(initial?.gradientMax, gradientMaxDefault));
  const [gradientMin, setGradientMin] = useState(() => savedNumber(initial?.gradientMin, gradientMinDefault));
  const [gradientMax, setGradientMax] = useState(() => savedNumber(initial?.gradientMax, gradientMaxDefault));
  const [cells, setCells] = useState<Map<string, MatrixCellRef>>(new Map());
  const [resultIdentity, setResultIdentity] = useState<string | null>(null);
  const modelIdentity = JSON.stringify({ setId, direction, timeRange: timeRangeParams(timeRange), shift: appliedTimeShift(scope), gate: appliedPriceGate(scope),
    clusterize, ...(clusterize ? { maturity, clusterGapHours } : {}),
    mode: trendMode, bend: trendBendDays, forecast, lambda: trendBlendLambda, cutoff: forecast ? observationCutoff : undefined });
  const currentIdentity = useRef(modelIdentity);
  const [metrics, setMetrics] = useState<SelectionMetrics | null>(null);
  const [running, setRunning] = useState(false);
  const [pendingCell, setPendingCell] = useState<string | null>(null);
  const [progress, setProgress] = useState({ done: 0, total: 0 });
  const [error, setError] = useState<string | null>(null);
  const runIdRef = useRef(0);
  const [displayIdentity, setDisplayIdentity] = useState(modelIdentity);
  if (displayIdentity !== modelIdentity) {
    setDisplayIdentity(modelIdentity);
    setRunning(false); setPendingCell(null); setProgress({ done: 0, total: 0 }); setError(null);
  }
  useLayoutEffect(() => {
    currentIdentity.current = modelIdentity;
    return () => { runIdRef.current += 1; };
  }, [modelIdentity]);

  useEffect(() => {
    reportState?.({ timeRange, timeShiftInput, considerPriceChange, priceChangeInput, gradientMin, gradientMax });
  }, [reportState, timeRange, timeShiftInput, considerPriceChange, priceChangeInput, gradientMin, gradientMax]);

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
    const stale = () => runIdRef.current !== runId || currentIdentity.current !== modelIdentity;
    try {
    const recipe = forecast !== undefined && trendMode !== "mean" ? buildForecastRecipe(forecast) : undefined;
    const lambda = Number(trendBlendLambda);
    if (trendMode === "blend" && !isTrendBlendLambda(lambda)) throw new Error("Доля прогноза λ: 0.05–0.95 с шагом 0.01.");
    const baseParams = {
      set: setId,
      direction,
      trendBendDays,
      trendMode,
      ...(recipe ? { forecast: recipe, observationCutoff } : {}),
      ...(trendMode === "blend" ? { trendBlendLambda: lambda } : {}),
      ...timeRangeParams(timeRange),
      ...(clusterize ? {
        setupMaturity: maturity,
        ...(clusterGapHours !== null && clusterGapHours > 0 ? { clusterGapHours } : {}),
      } : {}),
      ...(shiftMinutes > 0 ? { timeShiftMinutes: shiftMinutes } : {}),
      ...(priceChangePct !== null ? { priceChangePct } : {}),
    };
    setProgress({ done: 0, total: totalCells });
      const grid = await fetchMatrixGrid(baseParams, tpLevels, slLevels);
      if (stale()) return;
      const nextCells = new Map(grid.items.map((item) => [`${item.tpPct}:${item.slPct}`, item]));
      setCells(nextCells);
      setResultIdentity(modelIdentity);
      setMetrics(grid);
      setProgress({ done: nextCells.size, total: totalCells });
    } catch (cause) {
      if (!stale()) setError(cause instanceof Error ? cause.message : "ошибка расчёта");
    } finally {
      if (!stale()) {
        setRunning(false);
        setPendingCell(null);
      }
    }
  }, [setId, direction, timeRange, clusterize, maturity, clusterGapHours, tpLevels, slLevels, totalCells, trendBendDays, trendMode, forecast, trendBlendLambda, observationCutoff, modelIdentity]);

  const appliedShift = Math.max(0, Math.round(parseNumberInput(timeShiftInput) ?? 0));
  const appliedPriceChange = enablePriceChange && considerPriceChange ? (parseNumberInput(priceChangeInput) ?? 0) : null;

  const autoRunRef = useRef<() => void>(() => undefined);
  useEffect(() => {
    autoRunRef.current = () => { void run(appliedShift, appliedPriceChange); };
  });
  useEffect(() => {
    if (runToken === 0) return;
    autoRunRef.current();
  }, [runToken]);

  const commitGradientMin = () => { setGradientMin(gradientMinDraft); onCommit?.(); };
  const commitGradientMax = () => { setGradientMax(gradientMaxDraft); onCommit?.(); };

  return <section className="grid min-w-0 gap-4">
      <div className="flex items-center gap-2">
        <h2 className="text-lg font-semibold">{trendMode === "blend" ? "Смесь среднего и прогноза" : trendMode === "forecast" ? forecast?.method === "decayed_probability" ? "Прогноз матожидания" : "Прогноз матожидания по тренду" : "Среднее матожидание сетапов"}</h2>
        <InfoNote>{description}</InfoNote>
        <InfoNote>{trendMode === "mean" ? "Ячейка — матожидание в единицах риска: среднее движение по закрытым сделкам / SL. «—» — нет закрытых сделок." : forecast?.method === "decayed_probability" ? "Матрица показывает decisionR выбранного режима. Подсказка отдельно содержит среднее M, чистый прогноз F, реальные закрытия, prior и cutoff. Prior не выполняет минимум реальных закрытий." : "Прогноз — сглаженный уровень МО на последней дате с закрытыми сделками, без экстраполяции. Подсказка содержит SE и 95% неопределённость уровня, не будущей сделки."} Спорная свеча считается Stop Loss. Цвет жирных значений интерполируется от красного к зелёному между порогами ползунков.</InfoNote>
      </div>
      <div className="flex flex-wrap items-end gap-3">
        <TimeRangeControls setId={setId} setBounds={setBounds} value={timeRange} onChange={setTimeRange} onCommit={onCommit} />
        <Field label="Time-shift, мин" className="w-32">
          {(control) => <Input {...control} inputMode="numeric" value={timeShiftInput} onBlur={() => onCommit?.()} onChange={(event) => onScopeChange({ ...scope, timeShiftInput: event.target.value })} />}
        </Field>
        {enablePriceChange && (
          <>
            <label className="flex items-center gap-2 text-sm">
              <Checkbox checked={considerPriceChange} onCheckedChange={(checked) => { onScopeChange({ ...scope, considerPriceChange: checked === true }); onCommit?.(); }} />
              Учитывать смещение цены
            </label>
            <Field label="Смещение цены, %" className="w-36">
              {(control) => <Input {...control} inputMode="decimal" disabled={!considerPriceChange} value={priceChangeInput} onBlur={() => onCommit?.()} onChange={(event) => onScopeChange({ ...scope, priceChangeInput: event.target.value })} />}
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
            onPointerUp={commitGradientMin}
            onKeyUp={commitGradientMin}
            onBlur={commitGradientMin}
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
            onPointerUp={commitGradientMax}
            onKeyUp={commitGradientMax}
            onBlur={commitGradientMax}
          />
          <span className="tabular-nums">{gradientMaxDraft.toFixed(2)}</span>
        </label>
      </div>
      {error && <Alert variant="destructive" role="alert"><AlertDescription>{error}</AlertDescription></Alert>}
      {metrics !== null && resultIdentity === modelIdentity && (
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
                  const cell = resultIdentity === modelIdentity ? cells.get(`${tp}:${sl}`) : undefined;
                  if (cell === undefined) {
                    if (running && pendingCell === `${tp}:${sl}`) {
                      return <TableCell key={sl} numeric className="text-muted-foreground"><Spinner label="Расчёт ячейки" /></TableCell>;
                    }
                    return <TableCell key={sl} numeric className="text-muted-foreground/60">{running ? "…" : "·"}</TableCell>;
                  }
                  if (cell.riskExpectancy === null) {
                    return <TableCell key={sl} numeric className="text-muted-foreground" title={cell.forecastEstimate?.status === "unavailable" ? `Прогноз недоступен: ${cell.forecastEstimate.reason}` : "avg —% · R —"}>—</TableCell>;
                  }
                  return <TableCell
                    key={sl}
                    numeric
                    className={`tabular-nums font-bold analytics-cell-text-${matrixCellTextStep(cell.riskExpectancy, gradientMin, gradientMax)}`}
                    title={cell.forecast ? `${forecastRecipeLabel(cell.forecast, trendBendDays)} · M ${cell.meanR?.toFixed(3) ?? "—"} R · F ${cell.forecastR?.toFixed(3) ?? "—"} R · decisionR ${cell.decisionR?.toFixed(3) ?? "—"} R · C ${cell.observationCutoff}${cell.forecastEstimate?.status === "available" ? ` · реальные TP ${cell.forecastEstimate.value.tpCount}, SL ${cell.forecastEstimate.value.slCount}, W ${cell.forecastEstimate.value.weightSum.toFixed(3)}` : ""}` : `avg ${cell.avgMovePct?.toFixed(2) ?? "—"}% · R ${cell.riskExpectancy.toFixed(3)}${trendMode === "forecast" ? ` · SE ${cell.endpointSe?.toFixed(3) ?? "—"} R · 95% ±${cell.endpointSe != null ? (1.96 * cell.endpointSe).toFixed(3) : "—"} R` : ""}`}
                  >
                    {cell.riskExpectancy.toFixed(2)}
                  </TableCell>;
                })}
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </div>
  </section>;
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

interface AnalyticsSelectionScope {
  timeRange: TimeRangeValue;
  timeShiftInput: string;
  considerPriceChange: boolean;
  priceChangeInput: string;
}
function restoreSelectionScope(saved?: Record<string, unknown>): AnalyticsSelectionScope {
  return { timeRange: normalizeTimeRange(saved?.timeRange), timeShiftInput: savedString(saved?.timeShiftInput, "0"),
    considerPriceChange: saved?.considerPriceChange === true, priceChangeInput: savedString(saved?.priceChangeInput, "0") };
}
const appliedTimeShift = (scope: AnalyticsSelectionScope) => Math.max(0, Math.round(parseNumberInput(scope.timeShiftInput) ?? 0));
const appliedPriceGate = (scope: AnalyticsSelectionScope) => scope.considerPriceChange ? parseNumberInput(scope.priceChangeInput) ?? 0 : null;

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

/** Restores a saved time range from an untrusted JSON payload. */
function normalizeTimeRange(raw: unknown): TimeRangeValue {
  if (raw === null || typeof raw !== "object") return { mode: "all", month: "all", fromDay: null, toDay: null };
  const value = raw as Partial<TimeRangeValue>;
  if (value.mode === "month" && typeof value.month === "string" && /^\d{4}-\d{2}$/.test(value.month)) {
    return { mode: "month", month: value.month, fromDay: null, toDay: null };
  }
  if (value.mode === "custom" && typeof value.fromDay === "string" && typeof value.toDay === "string") {
    return { mode: "custom", month: "custom", fromDay: value.fromDay, toDay: value.toDay };
  }
  return { mode: "all", month: "all", fromDay: null, toDay: null };
}

function savedString(raw: unknown, fallback: string): string {
  return typeof raw === "string" && raw !== "" ? raw : fallback;
}

function savedNumber(raw: unknown, fallback: number): number {
  return typeof raw === "number" && Number.isFinite(raw) ? raw : fallback;
}

/** Month selector + date pair: month choice auto-fills the dates, manual dates set the dash. */
function TimeRangeControls({ setId, setBounds, value, onChange, onCommit }: {
  setId: string;
  setBounds: { from: string; to: string } | null;
  value: TimeRangeValue;
  onChange: (next: TimeRangeValue) => void;
  onCommit?: (() => void) | undefined;
}) {
  const [months, setMonths] = useState<string[]>([]);
  useEffect(() => {
    let cancelled = false;
    void fetchAnalyticsMonths(setId)
      .then((items) => { if (!cancelled) setMonths(items); })
      .catch(() => undefined);
    return () => { cancelled = true; };
  }, [setId]);
  // The selected month must exist as an option even before the fetched list arrives,
  // otherwise the browser cannot apply the controlled value and falls back to «Всё время».
  const monthOptions = value.mode === "month" && !months.includes(value.month) ? [value.month, ...months] : months;
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
          if (next === "all") {
            onChange({ mode: "all", month: "all", fromDay: null, toDay: null });
            onCommit?.();
          } else if (next !== "custom") {
            onChange({ mode: "month", month: next, fromDay: null, toDay: null });
            onCommit?.();
          }
        }}
      >
        <option value="all">Всё время</option>
        {monthOptions.map((month) => <option key={month} value={month}>{month}</option>)}
        <option value="custom" disabled>—</option>
      </Select>}
    </Field>
    <Field label="С даты" className="w-40">
      {(control) => <Input {...control} type="date" value={fromValue} onBlur={() => onCommit?.()} onChange={(event) => onChange({ mode: "custom", month: "custom", fromDay: event.target.value, toDay: toValue })} />}
    </Field>
    <Field label="По дату" className="w-40">
      {(control) => <Input {...control} type="date" value={toValue} onBlur={() => onCommit?.()} onChange={(event) => onChange({ mode: "custom", month: "custom", fromDay: fromValue, toDay: event.target.value })} />}
    </Field>
  </>;
}

interface TradeRowWithSource extends TradeRow {
  fromCache: boolean;
}

/** Interface calendar presentation of unchanged UTC aggregates and the shared Rust curve. */
function DailyExpectancyChart({ items, originUnix, meanRisk, winRate, trendBendDays, forecast, trendMode, trendBlendLambda, observedMode, onObservedModeChange }: {
  items: DailyExpectancyPoint[]; originUnix: number; meanRisk: number | null; winRate: number | null; trendBendDays: TrendBendDays;
  forecast: ForecastRecipeDraft | undefined; trendMode: TrendMode; trendBlendLambda: string;
  observedMode: ObservedDailyMode; onObservedModeChange: (mode: ObservedDailyMode) => void;
}) {
  const modelInput = useMemo(() => {
    try {
      const recipe = buildForecastRecipe(forecast ?? { method: "trend", version: 1, bendDays: trendBendDays });
      if (recipe.method === "decayed_probability") throw new Error("Для тренда требуется trend-рецепт.");
      const lambda = Number(trendBlendLambda);
      if (trendMode === "blend" && !isTrendBlendLambda(lambda)) throw new Error("Доля прогноза λ: 0.05–0.95 с шагом 0.01.");
      const projection: TrendProjectionRequest = { forecast: recipe, meanR: meanRisk, trendMode,
        ...(trendMode === "blend" ? { trendBlendLambda: lambda } : {}) };
      return { points: items.map((item) => ({ day: item.dayStartUnix / 86400,
        value: recipe.method === "trend_sum_r" ? item.sumR ?? (item.riskExpectancy === null ? null : item.riskExpectancy * item.closedCount) : item.riskExpectancy,
        weight: item.computedCount })), bend: recipe.bendDays, projection, items, meanRisk, winRate, error: null };
    } catch (cause) { return { error: cause instanceof Error ? cause.message : "Некорректный рецепт тренда." }; }
  }, [items, meanRisk, winRate, trendBendDays, forecast, trendMode, trendBlendLambda]);
  const [result, setResult] = useState<{ input: typeof modelInput; response: TrendProjectionResponse | null; error: string | null } | null>(null);
  useEffect(() => {
    if (modelInput.error !== null || !modelInput.points || !modelInput.projection || modelInput.bend === undefined || items.length === 0) return;
    const controller = new AbortController();
    void fetchDailyTrend(modelInput.points, modelInput.bend, controller.signal, modelInput.projection)
        .then((response) => { if (!controller.signal.aborted) setResult({ input: modelInput, response, error: null }); })
        .catch((error: unknown) => { if (!controller.signal.aborted) setResult((previous) => ({ input: previous?.response ? previous.input : modelInput, response: previous?.response ?? null, error: error instanceof Error ? error.message : "ошибка расчёта" })); });
    return () => { controller.abort(); };
  }, [modelInput, items.length]);
  // The keyed chart owns one scope. Publish observations and fit together, keeping
  // its last accepted snapshot while the next calculation iteration is fitting.
  const current = result;
  const displayed = current?.response ? current.input : modelInput;
  const displayedItems = displayed.items ?? items;
  const observed = useMemo(() => observedDailyPoints(displayedItems, observedMode), [displayedItems, observedMode]);
  const displayedMean = displayed.error === null ? displayed.meanRisk : meanRisk;
  const displayedWinRate = displayed.error === null ? displayed.winRate : winRate;
  const trend = current?.response?.trend ?? null;
  const projection = current?.response?.projection ?? null;
  if (displayedItems.length === 0) {
    return <p className="text-xs text-muted-foreground">График появится после расчёта первых строк.</p>;
  }
  const model = buildDailyExpectancyModel(observed, originUnix);
  const values = observed.map((item) => item.riskExpectancy).filter((value): value is number => value !== null && Number.isFinite(value));
  if (values.length === 0) {
    return <p className="text-xs text-muted-foreground">Пока нет дней с закрытыми TP/SL сделками.</p>;
  }
  // Scale to displayed values only, including fitted extrema but not hidden uncertainty.
  if (trend) for (const point of trend.curve) values.push(point.value);
  if (projection) for (const point of projection.curve) if (point.value !== null) values.push(point.value);
  const width = 760;
  const height = 280;
  const left = 64;
  const right = 24;
  const top = 24;
  const bottom = 56;
  const plotWidth = width - left - right;
  const plotHeight = height - top - bottom;
  const yMin = Math.min(0, ...values);
  const yMax = Math.max(1, ...values);
  const yRange = yMax - yMin;
  const firstX = model.points[0]!.x;
  const xRange = model.points[model.points.length - 1]!.x - firstX;
  const xForDay = (x: number) => left + (xRange === 0 ? plotWidth / 2 : ((x - firstX) / xRange) * plotWidth);
  // Map only the displayed calendar date of each original bucket instant. Neither
  // daily aggregates nor fine server-curve coordinates are shifted or refitted.
  const xForTrend = (day: number) => xForDay(day - model.originUnix / 86400);
  const timezone = effectiveTimezone();
  const yForValue = (value: number) => top + ((yMax - value) / yRange) * plotHeight;
  const zeroY = yForValue(0);
  const labelStep = Math.max(1, Math.ceil(displayedItems.length / 8));
  // Round presentation only so floating-point residuals cannot produce a -0.00R label.
  const fittedLabel = (value: number) => formatRisk(Number(value.toFixed(2)));

  return (
    <div className="grid gap-2">
    <h3 className="text-sm font-medium">Дневное матожидание в рисках · Среднее МО: <span aria-label="Среднее МО за период" className={displayedMean !== null && displayedMean > 0 ? "text-success" : displayedMean !== null && displayedMean < 0 ? "text-danger" : "text-muted-foreground"}>{displayedMean === null ? "—" : fittedLabel(displayedMean)}</span> · Винрейт: <span aria-label="Винрейт за период">{displayedWinRate === null ? "—" : `${displayedWinRate.toFixed(1)}%`}</span></h3>
    <DailyObservedControls value={observedMode} onChange={onObservedModeChange} />
    <p className="text-xs text-muted-foreground">Точки: {observedMode === "mean" ? "среднее R закрытых сделок дня" : "сумма R закрытых сделок дня"}. Модель: {forecast?.method === "trend_sum_r" ? "тренд дневной суммы R, не результата одной сделки" : "тренд среднего по дням"}. {trendMode === "mean" ? "Основная линия M: среднее за период." : trendMode === "blend" ? `Основная линия B: смесь M/F, λ ${trendBlendLambda}.` : "Основная линия F: чистый прогноз."}</p>
    {projection && <p className="text-xs text-muted-foreground">Чистый F: <span aria-label="Чистый прогноз тренда">{projection.forecastR === null ? "—" : fittedLabel(projection.forecastR)}</span> · decisionR: <span aria-label="Метрика решения тренда">{projection.decisionR === null ? "—" : fittedLabel(projection.decisionR)}</span></p>}
    {(modelInput.error ?? current?.error) && <p role="status" className="text-xs text-danger">Не удалось рассчитать тренд: {modelInput.error ?? current?.error}</p>}
    <p role="status" className="min-h-8 text-xs text-muted-foreground">{current?.input !== modelInput && !current?.error ? `Обновление графика…${current?.response ? " Показана предыдущая завершённая итерация." : ""}` : ""}</p>
    {projection?.decisionR === null && <p role="status" className="text-xs text-muted-foreground">Выбранная модель недоступна: недостаточно конечных дневных данных.</p>}
    <svg className="w-full" viewBox={`0 0 ${width} ${height}`} role="img" aria-label="Дневное матожидание в рисках">
      <desc>Серверные дневные агрегаты показаны по календарным суткам {timezone} без пересчёта исходных значений. Расстояния по оси X соответствуют календарным суткам; дни без закрытых сделок не равны нулю.</desc>
      <line className="stroke-muted-foreground/40" x1={left} y1={top} x2={left} y2={height - bottom} />
      <line className="stroke-muted-foreground/40" x1={left} y1={height - bottom} x2={width - right} y2={height - bottom} />
      <line className="stroke-muted-foreground/50" x1={left} y1={zeroY} x2={width - right} y2={zeroY} />
      <text className="fill-muted-foreground text-[10px]" x={left - 10} y={zeroY + 4} textAnchor="end" aria-label="Нулевой уровень">0</text>
      {[yMin, (yMin + yMax) / 2, yMax].filter((value) => value !== 0 && (value === yMin || value === yMax || Math.abs(yForValue(value) - zeroY) >= 14)).map((value) => (
        <g key={value.toFixed(4)}>
          <line className="stroke-muted-foreground/20" x1={left} y1={yForValue(value)} x2={width - right} y2={yForValue(value)} />
          <text className="fill-muted-foreground text-[10px]" x={left - 10} y={yForValue(value) + 4} textAnchor="end">{formatRisk(value)}</text>
        </g>
      ))}
      <DailyCalendarGrid fromUnix={model.points[0]!.dayStartUnix} toUnix={model.points[model.points.length - 1]!.dayStartUnix}
        x={(atUnix) => xForDay((atUnix - model.originUnix) / 86400)} top={top} bottom={height - bottom} />
      {model.segments.map((segment, index) => segment.length > 1
        ? <polyline key={index} className="fill-none stroke-primary stroke-2" points={segment.map((point) => `${xForDay(point.x)},${yForValue(point.riskExpectancy!)}`).join(" ")} /> : null)}
      {trend !== null ? <>
        <polyline data-testid="trend-curve" className="fill-none stroke-warning" strokeWidth={2} strokeDasharray="4 4"
          points={trend.curve.map((point) => `${xForTrend(point.day)},${yForValue(point.value)}`).join(" ")}>
          <title>{`${trend.method === "p_spline" ? "P-spline" : "Взвешенная прямая"}: изменение ${formatRisk(trend.end.value - trend.start.value)}, текущий уровень ${formatRisk(trend.end.value)}, SE ${trend.endpointSe?.toFixed(3) ?? "—"} R. Без экстраполяции.`}</title>
        </polyline>
        <text className="fill-warning font-mono text-[10px]" x={xForTrend(trend.start.day)} y={yForValue(trend.start.value) - 10} textAnchor="start" aria-label={`Начало тренда: ${fittedLabel(trend.start.value)}`}>{fittedLabel(trend.start.value)}</text>
        <text className="fill-warning font-mono text-[10px]" x={xForTrend(trend.end.day)} y={yForValue(trend.end.value) - 10} textAnchor="end" aria-label={`Конец тренда: ${fittedLabel(trend.end.value)}`}>{fittedLabel(trend.end.value)}</text>
      </> : null}
      {projection?.decisionR != null && <polyline data-testid="trend-decision-curve" data-decision-r={projection.decisionR} className="fill-none stroke-warning stroke-2"
        points={projection.curve.filter((point) => point.value !== null).map((point) => `${xForTrend(point.day)},${yForValue(point.value!)}`).join(" ")}>
        <title>{`${projection.trendMode === "mean" ? "M: среднее за период" : projection.trendMode === "blend" ? "B: смесь среднего и прогноза" : "F: чистый прогноз"}: ${fittedLabel(projection.decisionR)}`}</title>
      </polyline>}
      {model.points.map((item, index) => {
        const x = xForDay(item.x);
        const showLabel = index % labelStep === 0 || index === displayedItems.length - 1;
        return (
          <g key={item.day}>
            {item.riskExpectancy !== null && Number.isFinite(item.riskExpectancy) ? (
              <circle className={item.riskExpectancy >= 0 ? "fill-success" : "fill-danger"} cx={x} cy={yForValue(item.riskExpectancy)} r={4}>
                <title>{`${formatUnix(item.dayStartUnix, { time: false })} (${timezone}): ${observedMode === "sum" ? "сумма " : ""}${formatRisk(item.riskExpectancy)}, закрыто ${item.closedCount}, TP ${item.tpCount}, SL ${item.slCount}, из кеша ${item.cachedCount}/${item.computedCount}`}</title>
              </circle>
            ) : null}
            {showLabel ? <text className="fill-muted-foreground text-[10px]" x={x} y={height - 26} textAnchor="middle">{formatUnix(item.dayStartUnix, { time: false })}</text> : null}
          </g>
        );
      })}
      <text className="fill-muted-foreground text-[10px]" x={left + plotWidth / 2} y={height - 8} textAnchor="middle">Дата сетапа ({timezone})</text>
      <text className="fill-muted-foreground text-[10px]" x={left} y={top - 10}>{observedMode === "mean" ? "Среднее R дня" : "Сумма R дня"}</text>
    </svg>
    </div>
  );
}

interface TradesSectionProps {
  scope: AnalyticsSelectionScope;
  trendBendDays: TrendBendDays;
  trendMode: TrendMode;
  forecast: ForecastRecipeDraft | undefined;
  trendBlendLambda: string;
  observationCutoff: number;
  setPeriod: { fromUnix: number; toUnix: number } | null;
  setId: string;
  description: string;
  direction: "short" | "long";
  clusterize: boolean;
  maturity: number;
  clusterGapHours: number | null;
  setBounds: { from: string; to: string } | null;
  initial?: Record<string, unknown> | undefined;
  reportState?: ((snapshot: Record<string, unknown>) => void) | undefined;
  onCommit?: (() => void) | undefined;
}

function TradesSection({ setId, description, direction, clusterize, maturity, clusterGapHours, setBounds, initial, reportState, onCommit, trendBendDays, trendMode, forecast, trendBlendLambda, observationCutoff, setPeriod, scope }: TradesSectionProps) {
  const [tpInput, setTpInput] = useState(() => savedString(initial?.tpInput, "4"));
  const [slInput, setSlInput] = useState(() => savedString(initial?.slInput, "4"));
  const { timeRange } = scope;
  const shift = appliedTimeShift(scope);
  const gate = appliedPriceGate(scope);
  const [riskPctInput, setRiskPctInput] = useState(() => savedString(initial?.riskPctInput, "1"));
  const [rows, setRows] = useState<TradeRowWithSource[]>([]);
  const [observedMode, setObservedMode] = useState<ObservedDailyMode>("mean");
  const [resultParamsKey, setResultParamsKey] = useState<string | null>(null);
  const [filteredCount, setFilteredCount] = useState(0);
  const [running, setRunning] = useState(false);
  const [progress, setProgress] = useState({ done: 0, total: 0 });
  const [error, setError] = useState<string | null>(null);
  const runIdRef = useRef(0);
  const runRequest = useRef<AbortController | null>(null);

  useEffect(() => {
    reportState?.({ tpInput, slInput, riskPctInput });
  }, [reportState, tpInput, slInput, riskPctInput]);

  const params = useMemo(() => ({
    set: setId,
    direction,
    tpPct: parseNumberInput(tpInput) ?? 4,
    slPct: parseNumberInput(slInput) ?? 4,
    ...(shift > 0 ? { timeShiftMinutes: shift } : {}),
    ...timeRangeParams(timeRange),
    ...(clusterize ? {
      setupMaturity: maturity,
      ...(clusterGapHours !== null && clusterGapHours > 0 ? { clusterGapHours } : {}),
    } : {}),
    ...(gate !== null ? { priceChangePct: gate } : {}),
  }), [setId, direction, tpInput, slInput, shift, timeRange, clusterize, maturity, clusterGapHours, gate]);
  const paramsKey = JSON.stringify({ ...params, observationCutoff });
  const currentParamsKey = useRef(paramsKey);
  const [displayParamsKey, setDisplayParamsKey] = useState(paramsKey);
  if (displayParamsKey !== paramsKey) {
    setDisplayParamsKey(paramsKey);
    setRunning(false); setRows([]); setFilteredCount(0); setProgress({ done: 0, total: 0 }); setError(null);
  }
  useLayoutEffect(() => {
    currentParamsKey.current = paramsKey;
    return () => { runIdRef.current += 1; runRequest.current?.abort(); };
  }, [paramsKey]);

  const run = useCallback(async () => {
    const runId = runIdRef.current + 1;
    runIdRef.current = runId;
    runRequest.current?.abort();
    const controller = new AbortController(); runRequest.current = controller;
    setRunning(true);
    setError(null);
    setResultParamsKey(paramsKey);
    setProgress({ done: 0, total: 0 });
    const stale = () => controller.signal.aborted || runIdRef.current !== runId || currentParamsKey.current !== paramsKey;
    try {
      const selection = await fetchAnalyticsSetups(params, controller.signal);
      if (stale()) return;
      const setups = selection.setups;
      setProgress({ done: 0, total: setups.length });
      const bulk = await fetchTradeRows(params, controller.signal);
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
      for (let index = 0; index < setups.length;) {
        if (stale()) return;
        // Reuse cached runs in one cheap iteration; cold reads have a fixed fanout
        // of four. Promise.all preserves source order for the deposit calculation.
        let end = index;
        while (end < Math.min(setups.length, index + 64) && cachedByKey.has(`${setups[end]!.symbol}:${setups[end]!.closeTimeUnix}`)) end += 1;
        if (end === index) end = Math.min(setups.length, index + 4);
        const batch = await Promise.all(setups.slice(index, end).map(async (setup): Promise<TradeRowWithSource> => {
          const cached = cachedByKey.get(`${setup.symbol}:${setup.closeTimeUnix}`);
          return cached ? { ...cached, fromCache: true } : { ...await fetchTradeRow(setup, params, controller.signal), fromCache: false };
        }));
        if (stale()) return;
        collected.push(...batch);
        filtered += batch.filter((row) => row.status === "price_change_filtered").length;
        done += batch.length;
        flush();
        index = end;
      }
      if (setups.length === 0) flush();
    } catch (cause) {
      controller.abort();
      if (runIdRef.current === runId && currentParamsKey.current === paramsKey) setError(cause instanceof Error ? cause.message : "ошибка расчёта");
    } finally {
      if (runIdRef.current === runId && currentParamsKey.current === paramsKey) { setRunning(false); runRequest.current = null; }
    }
  }, [params, paramsKey]);

  const effectiveDays = timeRangeBounds(timeRange, setBounds);
  const effectiveFromDay = effectiveDays?.from;
  const effectiveToDay = effectiveDays?.to;
  const scopedRows = useMemo(() => {
    if (resultParamsKey !== paramsKey) return [];
    if (effectiveFromDay === undefined || effectiveToDay === undefined) return rows;
    const from = Math.floor(new Date(`${effectiveFromDay}T00:00:00Z`).getTime() / 1000);
    const to = Math.floor(new Date(`${effectiveToDay}T23:59:59Z`).getTime() / 1000);
    return rows.filter((row) => row.setup.closeTimeUnix >= from && row.setup.closeTimeUnix <= to);
  }, [rows, effectiveFromDay, effectiveToDay, resultParamsKey, paramsKey]);
  const visibleRows = useMemo(() => scopedRows.filter((row) => row.status !== "price_change_filtered"), [scopedRows]);

  const deposit = useMemo(() => {
    const riskPct = parseNumberInput(riskPctInput);
    if (riskPct === null || !Number.isFinite(riskPct) || riskPct <= 0 || riskPct > 100) {
      return { values: new Map<string, number>(), error: "Риск на сделку должен быть числом больше 0 и не больше 100" };
    }
    const values = new Map<string, number>();
    let multiplier = 1;
    for (const row of visibleRows) {
      if (row.riskReturn === null) continue;
      multiplier *= 1 + (riskPct / 100) * row.riskReturn;
      if (!Number.isFinite(multiplier) || multiplier < 0) {
        return { values, error: "Расчёт депозита остановлен: результат стал некорректным числом" };
      }
      values.set(`${row.setup.symbol}:${row.setup.closeTimeUnix}`, multiplier);
    }
    return { values, error: null as string | null };
  }, [visibleRows, riskPctInput]);

  const summary = useMemo(() => {
    const closed = visibleRows.filter((row) => row.status === "tp" || row.status === "sl");
    const tp = closed.filter((row) => row.status === "tp");
    const risks = closed.map((row) => row.riskReturn).filter((value): value is number => value !== null);
    const avgRisk = risks.length > 0 ? risks.reduce((a, b) => a + b, 0) / risks.length : null;
    return { closed: closed.length, winRate: closed.length > 0 ? (tp.length / closed.length) * 100 : null, avgRisk };
  }, [visibleRows]);

  const dailyPoints = useMemo(() => buildDailyExpectancy(visibleRows), [visibleRows]);
  const chartOriginUnix = effectiveDays !== null ? Date.parse(`${effectiveDays.from}T00:00:00Z`) / 1000 : dailyPoints[0]?.dayStartUnix ?? 0;
  const chartFromUnix = timeRange.mode === "all" && setPeriod ? setPeriod.fromUnix : chartOriginUnix;
  // Set bounds are inclusive raw-signal timestamps; the model API uses [from,to).
  const chartToUnix = timeRange.mode === "all" && setPeriod ? setPeriod.toUnix + 1 : effectiveToDay ? Date.parse(`${effectiveToDay}T00:00:00Z`) / 1000 + 86400 : (dailyPoints.at(-1)?.dayStartUnix ?? chartFromUnix) + 86400;
  const activeCutoff = forecast !== undefined && trendMode !== "mean";
  const retainFutureCohorts = forecast?.method !== "decayed_probability";
  const modelPopulation = useMemo(() => {
    try {
      return { days: activeCutoff && scopedRows.length > 0
        ? buildObservedForecastDays(scopedRows, chartFromUnix, chartToUnix, observationCutoff,
          { timeShiftMinutes: shift, retainFutureCohorts }) : dailyPoints, error: null };
    } catch (cause) { return { days: [], error: cause instanceof Error ? cause.message : "Не удалось подтвердить популяцию модели." }; }
  }, [activeCutoff, scopedRows, chartFromUnix, chartToUnix, observationCutoff, shift, retainFutureCohorts, dailyPoints]);
  const modelDays = modelPopulation.days;
  const modelMean = useMemo(() => {
    const closed = modelDays.reduce((sum, day) => sum + day.closedCount, 0);
    const tp = modelDays.reduce((sum, day) => sum + day.tpCount, 0);
    return { mean: closed > 0 ? modelDays.reduce((sum, day) => sum + (day.sumR ?? 0), 0) / closed : null,
      winRate: closed > 0 ? 100 * tp / closed : null };
  }, [modelDays]);
  const probabilityChart = useMemo((): { request: ForecastCurveRequest; days: DailyExpectancyPoint[]; error?: never } | { error: string } | null => {
    if (forecast?.method !== "decayed_probability" || scopedRows.length === 0 || modelPopulation.error !== null) return null;
    try {
      const recipe = buildForecastRecipe(forecast);
      const q = params.tpPct / params.slPct;
      if (!Number.isFinite(q) || q <= 0) throw new Error("TP и SL должны быть конечными числами больше 0.");
      const lambda = Number(trendBlendLambda);
      if (trendMode === "blend" && !isTrendBlendLambda(lambda)) throw new Error("Доля прогноза λ: 0.05–0.95 с шагом 0.01.");
      const fromUnix = chartFromUnix;
      const toUnix = chartToUnix;
      const days = modelDays;
      return { days, request: { forecast: recipe, trendMode, trendBlendLambda: trendMode === "blend" ? lambda : 0.7, q, fromUnix, toUnix, observationCutoff,
        atUnix: observationCutoff + 86400, daily: days.map((day) => ({ dayStartUnix: day.dayStartUnix, tpCount: day.tpCount, slCount: day.slCount })) } };
    } catch (cause) { return { error: cause instanceof Error ? cause.message : "Некорректные входы модели." }; }
  }, [forecast, scopedRows, modelPopulation.error, params.tpPct, params.slPct, trendBlendLambda, trendMode, chartFromUnix, chartToUnix, modelDays, observationCutoff]);
  const chartScopeKey = JSON.stringify([paramsKey, trendMode, forecast, trendBendDays, trendBlendLambda]);

  return <section className="grid min-w-0 gap-4">
      <div className="flex items-center gap-2">
        <h2 className="text-lg font-semibold">Расчёт ячейки</h2>
        <InfoNote>{description}</InfoNote>
      </div>
      <div className="flex flex-wrap items-end gap-3">
        <Field label="TP, %" className="w-28">
          {(control) => <Input {...control} inputMode="decimal" value={tpInput} onBlur={() => onCommit?.()} onChange={(event) => setTpInput(event.target.value)} />}
        </Field>
        <Field label="SL, %" className="w-28">
          {(control) => <Input {...control} inputMode="decimal" value={slInput} onBlur={() => onCommit?.()} onChange={(event) => setSlInput(event.target.value)} />}
        </Field>
        <Field label="Риск на сделку, %" className="w-32">
          {(control) => <Input {...control} inputMode="decimal" value={riskPctInput} onBlur={() => onCommit?.()} onChange={(event) => setRiskPctInput(event.target.value)} />}
        </Field>
        {running
          ? <Button variant="outline" onClick={() => { runIdRef.current += 1; runRequest.current?.abort(); runRequest.current = null; setRunning(false); }}>Остановить</Button>
          : <Button disabled={setId === ""} onClick={() => { void run(); }}>Рассчитать</Button>}
        {progress.total > 0 && <Badge variant={progress.done >= progress.total ? "success" : "info"}>Сделок: {progress.done} / {progress.total}</Badge>}
      </div>
      {error && <Alert variant="destructive" role="alert"><AlertDescription>{error}</AlertDescription></Alert>}
      {deposit.error && visibleRows.length > 0 && <p role="status" className="text-sm text-danger">{deposit.error}</p>}
      {modelPopulation.error !== null && <p role="status" className="text-sm text-danger">Модель недоступна: {modelPopulation.error}</p>}
      <div className="flex flex-wrap gap-2 text-xs">
        {summary.winRate !== null && <Badge variant={summary.avgRisk !== null && summary.avgRisk >= 0 ? "success" : "danger"}>Win rate: {summary.winRate.toFixed(1)}%</Badge>}
        {summary.avgRisk !== null && <Badge>Матожидание: {formatRisk(summary.avgRisk)} по {summary.closed} закрытым</Badge>}
        {filteredCount > 0 && <Badge variant="warning">Отфильтровано смещением цены: {filteredCount}</Badge>}
      </div>
      {forecast?.method === "decayed_probability" ? probabilityChart && ("error" in probabilityChart
        ? <p role="status" className="text-sm text-danger">Модель недоступна: {probabilityChart.error}</p>
        : <AnalyticsForecastChart key={chartScopeKey} retainWhileLoading request={probabilityChart.request} days={probabilityChart.days} observedMode={observedMode} onObservedModeChange={setObservedMode} />)
        : scopedRows.length > 0 && modelPopulation.error === null && <DailyExpectancyChart key={chartScopeKey} items={modelDays} originUnix={chartOriginUnix} meanRisk={modelMean.mean} winRate={modelMean.winRate}
          trendBendDays={trendBendDays} forecast={forecast} trendMode={trendMode} trendBlendLambda={trendBlendLambda} observedMode={observedMode} onObservedModeChange={setObservedMode} />}
      {visibleRows.length === 0 && !running && <p className="text-xs text-muted-foreground">График появится после ручного расчёта строк с закрытыми TP/SL сделками.</p>}
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
                <TableHead numeric>Депозит, х</TableHead>
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
                  <TableCell numeric>{deposit.values.get(`${row.setup.symbol}:${row.setup.closeTimeUnix}`)?.toFixed(2) ?? "—"}</TableCell>
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
  </section>;
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

  return <section className="grid min-w-0 gap-4">
      <div className="flex items-center gap-2">
        <h2 className="text-lg font-semibold">Кеш аналитики</h2>
        <InfoNote>Кеш результатов хранится в БД платформы рядом с сетапами. Догрузка истории не сбрасывает его автоматически; очистка удаляет только кеш аналитики, не трогая запуски, сетапы и рыночные данные.</InfoNote>
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
  </section>;
}

type AnalyticsSection = "short" | "long" | "cache";

export function AnalyticsPage() {
  useSyncExternalStore(subscribeInterfaceSettings, interfaceSettings, interfaceSettings);
  const sharedSettings = useSharedAnalyticsSettings();
  const settingsState = sharedSettings.status === "idle" ? "loading" : sharedSettings.status;
  const settingsError = sharedSettings.error;
  const saveStatus = sharedSettings.saveStatus;
  const [sets, setSets] = useState<AnalyticsSet[]>([]);
  const [setsState, setSetsState] = useState<"loading" | "ready" | "error">("loading");
  const [savedSettings, setSavedSettings] = useState<AnalyticsUserSettings | null>(null);
  const [shortScope, setShortScope] = useState<AnalyticsSelectionScope>(() => restoreSelectionScope());
  const [longScope, setLongScope] = useState<AnalyticsSelectionScope>(() => restoreSelectionScope());
  const [setId, setSetId] = useState("");
  const [section, setSection] = useState<AnalyticsSection>("short");
  const [clusterize, setClusterize] = useState(true);
  const [clusterGapInput, setClusterGapInput] = useState("8");
  const [maturityDraft, setMaturityDraft] = useState(0.5);
  const [maturity, setMaturity] = useState(0.5);
  const [autoCalc, setAutoCalc] = useState(false);
  const trendBendDays = sharedSettings.settings?.trendBendDays ?? "linear";
  const trendMode = sharedSettings.settings?.trendMode ?? "mean";
  const forecast = sharedSettings.settings?.forecast;
  const trendBlendLambda = sharedSettings.settings?.trendBlendLambda ?? "0.7";
  // One frozen cutoff for all matrix cells and the supplied-outcome graph scope.
  const [observationCutoff, setObservationCutoff] = useState(() => Math.floor(Date.now() / 1000));
  const [runToken, setRunToken] = useState(0);
  const sectionsRef = useRef<Partial<Record<AnalyticsSectionKey, Record<string, unknown>>>>({});
  const [hydrated, setHydrated] = useState(false);

  useEffect(() => {
    let cancelled = false;
    void fetchAnalyticsSets().then((items) => {
      if (cancelled) return;
      setSets(items); setSetsState("ready");
    }).catch(() => { if (!cancelled) setSetsState("error"); });
    return () => { cancelled = true; };
  }, []);

  const settings = sharedSettings.settings;
  // Seed once before mounting section forms. Keep-alive forecast edits thereafter
  // come directly from the shared owner, never from this bootstrap snapshot.
  if (!hydrated && settingsState === "ready" && settings !== null && setsState !== "loading") {
    setSetId(settings.setId !== null && sets.some((item) => item.setId === settings.setId) ? settings.setId : sets[0]?.setId ?? "");
    setClusterize(settings.clusterize);
    setMaturityDraft(settings.maturity);
    setMaturity(settings.maturity);
    setClusterGapInput(settings.clusterGapInput);
    setAutoCalc(settings.autoCalc);
    setSavedSettings(settings);
    setShortScope(restoreSelectionScope(settings.sections["short-matrix"]));
    setLongScope(restoreSelectionScope(settings.sections["long-matrix"]));
    if (settings.autoCalc) setRunToken(1);
    setHydrated(true);
  }

  const reportShortMatrix = useCallback((snapshot: Record<string, unknown>) => { sectionsRef.current["short-matrix"] = snapshot; }, []);
  const reportShortTrades = useCallback((snapshot: Record<string, unknown>) => { sectionsRef.current["short-trades"] = snapshot; }, []);
  const reportLongMatrix = useCallback((snapshot: Record<string, unknown>) => { sectionsRef.current["long-matrix"] = snapshot; }, []);
  const reportLongTrades = useCallback((snapshot: Record<string, unknown>) => { sectionsRef.current["long-trades"] = snapshot; }, []);

  const collectSettings = useCallback((): Partial<AnalyticsUserSettings> => ({
    setId: setId === "" ? null : setId,
    clusterize,
    maturity,
    clusterGapInput,
    autoCalc,
    sections: { ...sectionsRef.current },
  }), [setId, clusterize, maturity, clusterGapInput, autoCalc]);
  const collectRef = useRef(collectSettings);
  useEffect(() => { collectRef.current = collectSettings; }, [collectSettings]);

  /** Debounced autosave: coalesces slider commits and rapid consecutive changes.
   * matrixRelevant=false (edits inside the trade-list sections) saves without re-running matrices. */
  const commitTimerRef = useRef<number | null>(null);
  const pendingCommitRef = useRef({ saveOwned: false, matrixRelevant: false });
  useEffect(() => () => {
    if (commitTimerRef.current !== null) window.clearTimeout(commitTimerRef.current);
  }, []);
  const onSettingsCommit = useCallback((matrixRelevant = true, saveOwned = true) => {
    // Shared forecast fields already have their own writer/debounce. Observing
    // them must not save unrelated drafts or postpone an owned-settings commit
    // behind that writer, which would cause a stale first PUT and a second PUT.
    if (!saveOwned && !collectRef.current().autoCalc) return;
    pendingCommitRef.current.saveOwned ||= saveOwned;
    pendingCommitRef.current.matrixRelevant ||= matrixRelevant;
    if (commitTimerRef.current !== null) {
      if (!saveOwned) return;
      window.clearTimeout(commitTimerRef.current);
    }
    commitTimerRef.current = window.setTimeout(() => {
      commitTimerRef.current = null;
      const pending = pendingCommitRef.current;
      pendingCommitRef.current = { saveOwned: false, matrixRelevant: false };
      if (pending.saveOwned) updateSharedAnalyticsSettings(collectRef.current(), 0);
      if (pending.matrixRelevant && collectRef.current().autoCalc) setRunToken((token) => token + 1);
    }, 800);
  }, []);

  const sharedForecastIdentity = JSON.stringify({ trendMode, ...(trendMode === "mean" ? {} : {
    forecast, bend: forecast?.method === "decayed_probability" ? undefined : trendBendDays,
    lambda: trendMode === "blend" ? trendBlendLambda : undefined,
  }) });
  const lastSharedForecastIdentity = useRef<string | null>(null);
  useEffect(() => {
    if (!hydrated || settingsState !== "ready") return;
    const previous = lastSharedForecastIdentity.current;
    lastSharedForecastIdentity.current = sharedForecastIdentity;
    // Seed without an extra hydration run. Local and keep-alive Setups commits
    // use the same existing timer, so one commit produces one matrix batch/run.
    if (previous !== null && previous !== sharedForecastIdentity) onSettingsCommit(true, false);
  }, [hydrated, settingsState, sharedForecastIdentity, onSettingsCommit]);

  const commitMaturity = () => {
    setMaturity(maturityDraft);
    onSettingsCommit();
  };

  const appliedClusterGapHours = parseNumberInput(clusterGapInput);
  const activeSet = sets.find((set) => set.setId === setId);
  const setPeriod = useMemo(() => activeSet?.fromUnix != null && activeSet.toUnix != null ? { fromUnix: activeSet.fromUnix, toUnix: activeSet.toUnix } : null, [activeSet]);
  const setBounds = activeSet?.fromUnix != null && activeSet?.toUnix != null
    ? { from: utcDayOf(activeSet.fromUnix), to: utcDayOf(activeSet.toUnix) }
    : null;

  return <section aria-labelledby="analytics-heading" className="grid gap-4">
    <PageHeader id="analytics-heading" title="Аналитика" description="Backtest наборов сетапов из завершённых исторических запусков: матожидание TP/SL, зрелость кластеров, построчные закрытия. Расчёты читают минутные свечи через сервис данных; результаты кешируются." />
    <PageSidebar label="Подразделы аналитики" value={section} onValueChange={setSection} items={[
      { id: "short", label: "SHORT" }, { id: "long", label: "LONG" }, { id: "cache", label: "Кеш" },
    ]}>
    <fieldset disabled={settingsState !== "ready" || !hydrated} className="grid min-w-0 gap-4">
        <div className="grid gap-3">
          <h3 className="text-sm font-semibold text-muted-foreground">Набор и параметры запуска</h3>
          <div className="flex flex-wrap items-end gap-3">
            <Field label="Набор сетапов" className="w-full max-w-md">
              {(control) => (
                <Select {...control} searchable searchPlaceholder="Найти набор сетапов" value={setId} disabled={setsState !== "ready"} onChange={(event) => { setSetId(event.target.value); onSettingsCommit(); }}>
                  {sets.length === 0 && <option value="">Нет завершённых запусков</option>}
                  {hydrated && sets.map((set) => <option key={set.setId} value={set.setId}>{set.label}</option>)}
                </Select>
              )}
            </Field>
            {setsState === "ready" && activeSet !== undefined && (
              <div className="flex flex-wrap gap-2 text-xs self-center">
                <Badge>Сетапов: {activeSet.setupCount}</Badge>
                <Badge>{activeSet.exchange} · {activeSet.category}{activeSet.symbol === null ? "" : ` · ${activeSet.symbol}`}</Badge>
              </div>
            )}
            <label className="flex items-center gap-2 text-sm self-center">
              <Checkbox checked={autoCalc} onCheckedChange={(checked) => { setAutoCalc(checked === true); onSettingsCommit(); }} />
              Авторасчёт матриц
            </label>
          </div>
          {setsState === "ready" && activeSet !== undefined && (
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
          )}
        </div>
        <Separator />
        <div className="grid gap-3">
          <div className="flex items-center gap-2"><h3 className="text-sm font-semibold text-muted-foreground">Кластеризация</h3>
          <InfoNote>Настройки кластеризации действуют одновременно на все разделы SHORT и LONG. 0 — первый сетап кластера, 1 — последний, 0.5 — середина; соседние сигналы одной монеты с разрывом до {(appliedClusterGapHours ?? 8).toString()} часов образуют кластер. При выключенной галочке кластеризация не выполняется — в расчёт входят все сетапы.</InfoNote></div>
          <div className="flex flex-wrap items-end gap-3">
            <label className="flex items-center gap-2 text-sm">
              <Checkbox checked={clusterize} onCheckedChange={(checked) => { setClusterize(checked === true); onSettingsCommit(); }} />
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
                onPointerUp={commitMaturity}
                onKeyUp={commitMaturity}
                onBlur={commitMaturity}
              />}
            </Field>
            <Field label="Разрыв кластера, ч" className="w-32">
              {(control) => <Input {...control} inputMode="decimal" disabled={!clusterize} value={clusterGapInput} onBlur={() => onSettingsCommit()} onChange={(event) => setClusterGapInput(event.target.value)} />}
            </Field>
          </div>
        </div>
        <Separator />
        <div className="grid gap-3">
          <div className="flex items-center gap-2"><h3 className="text-sm font-semibold text-muted-foreground">Матожидание и модель прогноза</h3>
          <InfoNote>Режим mean по умолчанию. Среднее M, чистый прогноз F и decisionR смеси различаются. Probability использует только наблюдавшиеся TP/SL и единый cutoff C; за C продолжение плоское. Прежний trend доступен отдельно для сравнения; его fitted endpoint не является probability-моделью или третьей компонентой смеси. Ретроспективная кривая не доказывает OOS-качество.</InfoNote></div>
          <AnalyticsTrendControls scope="analytics" mode={trendMode} bend={trendBendDays}
            forecast={forecast} onForecastChange={(next) => { updateSharedAnalyticsSettings({ forecast: next, ...(next.method !== "decayed_probability" ? { trendBendDays: next.bendDays } : {}) }); onSettingsCommit(true, false); }}
            onBlendModeChange={() => { updateSharedAnalyticsSettings({ trendMode: "blend" }); onSettingsCommit(true, false); }} blendLambda={trendBlendLambda}
            onBlendLambdaChange={(lambda) => { updateSharedAnalyticsSettings({ trendBlendLambda: lambda }); onSettingsCommit(true, false); }}
            onModeChange={(mode) => { updateSharedAnalyticsSettings({ trendMode: mode }); onSettingsCommit(true, false); }}
            onBendChange={(bend) => { updateSharedAnalyticsSettings({ trendBendDays: bend }); onSettingsCommit(true, false); }} />
          {forecast && <div className="flex flex-wrap items-center gap-3 text-xs text-muted-foreground">
            <span>Cutoff C: {formatUnix(observationCutoff)} ({effectiveTimezone()}) · зафиксирован для матриц и модельного графика, не двигается по часам.</span>
            <Button type="button" variant="outline" onClick={() => { setObservationCutoff(Math.floor(Date.now() / 1000)); onSettingsCommit(); }}>Новый cutoff C</Button>
          </div>}
        </div>
        {saveStatus === "error" && <p role="status" className="text-xs text-danger">Не удалось сохранить настройки</p>}
    </fieldset>
    {(setsState === "loading" || settingsState === "loading") && <Spinner label="Загрузка наборов…" />}
    {settingsError && <Alert variant="destructive" role="alert">Не удалось восстановить настройки аналитики: {settingsError}. Сохранённый рецепт не заменён default; обновите страницу после исправления.</Alert>}
    {setsState === "error" && <Alert variant="destructive" role="alert">Наборы недоступны: не удалось получить список исторических запусков</Alert>}
    {setsState === "ready" && sets.length === 0 && (
      <EmptyState title="Нет наборов для аналитики" description="Аналитика работает по завершённым историческим запускам. Создайте запуск на странице «Запуски» и дождитесь его завершения." />
    )}
    {settingsState === "ready" && hydrated && (<>
    <PageSidebarPanel value="short">
      <MatrixSection
        scope={shortScope} onScopeChange={setShortScope}
        trendBendDays={trendBendDays}
        trendMode={trendMode}
        forecast={forecast} trendBlendLambda={trendBlendLambda} observationCutoff={observationCutoff}
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
        initial={savedSettings?.sections["short-matrix"]}
        reportState={reportShortMatrix}
        onCommit={onSettingsCommit}
        runToken={runToken}
      />
      <Separator />
      <TradesSection
        scope={shortScope}
        trendBendDays={trendBendDays}
        trendMode={trendMode} forecast={forecast} trendBlendLambda={trendBlendLambda} observationCutoff={observationCutoff} setPeriod={setPeriod}
        setId={setId}
        description="SHORT: построчные сделки — вход по close свечи через time-shift, закрытие по TP или SL за 7 дней. Сначала показываются кешированные строки, недостающие досчитываются."
        direction="short"
        clusterize={clusterize}
        maturity={maturity}
        clusterGapHours={appliedClusterGapHours}
        setBounds={setBounds}
        initial={savedSettings?.sections["short-trades"]}
        reportState={reportShortTrades}
        onCommit={() => onSettingsCommit(false)}
      />
    </PageSidebarPanel>
    <PageSidebarPanel value="long">
      <MatrixSection
        scope={longScope} onScopeChange={setLongScope}
        trendBendDays={trendBendDays}
        trendMode={trendMode}
        forecast={forecast} trendBlendLambda={trendBlendLambda} observationCutoff={observationCutoff}
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
        initial={savedSettings?.sections["long-matrix"]}
        reportState={reportLongMatrix}
        onCommit={onSettingsCommit}
        runToken={runToken}
      />
      <Separator />
      <TradesSection
        scope={longScope}
        trendBendDays={trendBendDays}
        trendMode={trendMode} forecast={forecast} trendBlendLambda={trendBlendLambda} observationCutoff={observationCutoff} setPeriod={setPeriod}
        setId={setId}
        description="LONG: построчные закрытия по уровням TP/SL с ценой входа, временем касания и результатом в рисках."
        direction="long"
        clusterize={clusterize}
        maturity={maturity}
        clusterGapHours={appliedClusterGapHours}
        setBounds={setBounds}
        initial={savedSettings?.sections["long-trades"]}
        reportState={reportLongTrades}
        onCommit={() => onSettingsCommit(false)}
      />
    </PageSidebarPanel>
    <PageSidebarPanel value="cache">
      <AnalyticsCacheCard />
    </PageSidebarPanel>
    </>)}
    </PageSidebar>
  </section>;
}
