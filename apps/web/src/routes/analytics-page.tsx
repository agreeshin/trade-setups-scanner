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

function matrixCellClass(risk: number | null): string {
  if (risk === null) return "bg-muted/40 text-muted-foreground";
  if (risk <= -0.5) return "bg-danger/20 text-danger";
  if (risk <= -0.05) return "bg-danger/10 text-danger";
  if (risk >= 0.2) return "bg-success/20 text-success";
  if (risk >= 0.05) return "bg-success/10 text-success";
  return "text-foreground";
}

interface MatrixSectionProps {
  setId: string;
  title: string;
  description: string;
  direction: "short" | "long";
  tpLevels: number[];
  slLevels: number[];
  enableMaturity: boolean;
  enableMonth: boolean;
  enablePriceChange: boolean;
}

function MatrixSection({
  setId, title, description, direction, tpLevels, slLevels, enableMaturity, enableMonth, enablePriceChange,
}: MatrixSectionProps) {
  const [maturityDraft, setMaturityDraft] = useState(0.5);
  const [maturity, setMaturity] = useState(0.5);
  const [month, setMonth] = useState("all");
  const [timeShiftInput, setTimeShiftInput] = useState("0");
  const [timeShift, setTimeShift] = useState(0);
  const [considerPriceChange, setConsiderPriceChange] = useState(false);
  const [priceChangeInput, setPriceChangeInput] = useState("0");
  const [priceChange, setPriceChange] = useState<number | null>(null);
  const [cells, setCells] = useState<Map<string, MatrixCellRef>>(new Map());
  const [metrics, setMetrics] = useState<SelectionMetrics | null>(null);
  const [running, setRunning] = useState(false);
  const [progress, setProgress] = useState({ done: 0, total: 0 });
  const [error, setError] = useState<string | null>(null);
  const runIdRef = useRef(0);

  const totalCells = tpLevels.length * slLevels.length;

  const run = useCallback(async (shiftMinutes: number, priceChangePct: number | null) => {
    const runId = runIdRef.current + 1;
    runIdRef.current = runId;
    setRunning(true);
    setError(null);
    const stale = () => runIdRef.current !== runId;
    const baseParams = {
      set: setId,
      direction,
      ...(enableMonth && month !== "all" ? { month } : {}),
      ...(enableMaturity ? { setupMaturity: maturity } : {}),
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
      if (!stale()) setRunning(false);
    }
  }, [setId, direction, month, maturity, enableMonth, enableMaturity, tpLevels, slLevels, totalCells]);

  useEffect(() => {
    if (setId === "") return;
    const timer = setTimeout(() => { void run(timeShift, priceChange); }, 400);
    return () => { clearTimeout(timer); runIdRef.current += 1; setRunning(false); };
  }, [setId, month, maturity, timeShift, priceChange, run]);

  const commitSlider = () => { setMaturity(maturityDraft); };

  return <Card>
    <CardContent className="grid gap-4">
      <div className="grid gap-1">
        <h2 className="text-lg font-semibold">{title}</h2>
        <p className="text-xs text-muted-foreground">{description}</p>
      </div>
      <div className="flex flex-wrap items-end gap-3">
        {enableMaturity && (
          <Field label={`Зрелость сетапа: ${maturityDraft.toFixed(1)}`} className="w-56">
            {(control) => <Input
              {...control}
              type="range"
              min={0}
              max={1}
              step={0.1}
              value={maturityDraft}
              onChange={(event) => setMaturityDraft(Number(event.target.value))}
              onPointerUp={commitSlider}
              onKeyUp={commitSlider}
              onBlur={commitSlider}
            />}
          </Field>
        )}
        {enableMonth && (
          <Field label="Месяц" className="w-44">
            {(control) => <Select {...control} value={month} onChange={(event) => setMonth(event.target.value)}>
              <option value="all">Всё время</option>
              <MonthOptions setId={setId} />
            </Select>}
          </Field>
        )}
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
        <Button variant="outline" disabled={running} onClick={() => {
          const shift = Math.max(0, Math.round(parseNumberInput(timeShiftInput) ?? 0));
          setTimeShiftInput(String(shift));
          setTimeShift(shift);
          const pct = considerPriceChange ? (parseNumberInput(priceChangeInput) ?? 0) : null;
          setPriceChange(pct);
          void run(shift, pct);
        }}>Рассчитать</Button>
        {progress.total > 0 && <Badge variant={progress.done >= progress.total ? "success" : "info"}>Ячеек: {progress.done} / {progress.total}</Badge>}
      </div>
      {enableMaturity && <p className="text-xs text-muted-foreground">0 — первый сетап кластера, 1 — последний, 0.5 — середина. Соседние сигналы одной монеты с разрывом до 8 часов образуют кластер.</p>}
      {error && <Alert variant="destructive" role="alert"><AlertDescription>{error}</AlertDescription></Alert>}
      {metrics !== null && (
        <div className="flex flex-wrap gap-2 text-xs">
          <Badge>После кластеризации: {metrics.totalMatchingSetups}</Badge>
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
                  return <TableCell key={sl} numeric className={cell ? matrixCellClass(cell.riskExpectancy) : "bg-muted/40 text-muted-foreground"} title={cell ? `avg ${cell.avgMovePct?.toFixed(2) ?? "—"}% · R ${cell.riskExpectancy?.toFixed(3) ?? "—"}` : undefined}>
                    {cell ? (cell.riskExpectancy === null ? "—" : cell.riskExpectancy.toFixed(2)) : running ? "…" : "·"}
                  </TableCell>;
                })}
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </div>
      <p className="text-xs text-muted-foreground">Ячейка — матожидание в единицах риска: среднее движение по закрытым сделкам / SL. «—» — нет закрытых сделок. Спорная свеча считается Stop Loss.</p>
    </CardContent>
  </Card>;
}

function MonthOptions({ setId }: { setId: string }) {
  const [months, setMonths] = useState<string[]>([]);
  useEffect(() => {
    if (setId === "") return;
    let cancelled = false;
    void fetchAnalyticsMonths(setId).then((items) => { if (!cancelled) setMonths(items); }).catch(() => undefined);
    return () => { cancelled = true; };
  }, [setId]);
  return <>{months.map((month) => <option key={month} value={month}>{month}</option>)}</>;
}

interface TradesSectionProps {
  setId: string;
  title: string;
  description: string;
  fixedDirection: "short" | "long" | null;
  enableMaturity: boolean;
  enablePriceChange: boolean;
}

function TradesSection({ setId, title, description, fixedDirection, enableMaturity, enablePriceChange }: TradesSectionProps) {
  const [direction, setDirection] = useState<"short" | "long">(fixedDirection ?? "long");
  const [tpInput, setTpInput] = useState("4");
  const [slInput, setSlInput] = useState("4");
  const [timeShiftInput, setTimeShiftInput] = useState("0");
  const [maturityDraft, setMaturityDraft] = useState(0.5);
  const [maturity, setMaturity] = useState(0.5);
  const [considerPriceChange, setConsiderPriceChange] = useState(false);
  const [priceChangeInput, setPriceChangeInput] = useState("0");
  const [fromDate, setFromDate] = useState("");
  const [toDate, setToDate] = useState("");
  const [rows, setRows] = useState<TradeRow[]>([]);
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
    ...(enableMaturity ? { setupMaturity: maturity } : {}),
    ...(enablePriceChange && considerPriceChange ? { priceChangePct: parseNumberInput(priceChangeInput) ?? 0 } : {}),
  }), [setId, direction, tpInput, slInput, timeShiftInput, maturity, enableMaturity, enablePriceChange, considerPriceChange, priceChangeInput]);

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
      const collected: TradeRow[] = [];
      let filtered = 0;
      let done = 0;
      const flush = () => {
        setRows([...collected]);
        setFilteredCount(filtered);
        setProgress({ done, total: setups.length });
      };
      for (const setup of setups) {
        const key = `${setup.symbol}:${setup.closeTimeUnix}`;
        let row = cachedByKey.get(key);
        if (row === undefined) {
          row = await fetchTradeRow(setup, params);
          if (stale()) return;
        }
        done += 1;
        if (row.status === "price_change_filtered") {
          filtered += 1;
        } else {
          collected.push(row);
        }
        flush();
      }
    } catch (cause) {
      if (!stale()) setError(cause instanceof Error ? cause.message : "ошибка расчёта");
    } finally {
      if (!stale()) setRunning(false);
    }
  }, [params]);

  const dateBounds = useMemo(() => {
    const from = fromDate === "" ? null : Math.floor(new Date(`${fromDate}T00:00:00`).getTime() / 1000);
    const to = toDate === "" ? null : Math.floor(new Date(`${toDate}T23:59:59`).getTime() / 1000);
    return { from, to };
  }, [fromDate, toDate]);

  const visibleRows = useMemo(() => rows.filter((row) => {
    const time = row.setup.closeTimeUnix;
    return (dateBounds.from === null || time >= dateBounds.from) && (dateBounds.to === null || time <= dateBounds.to);
  }), [rows, dateBounds]);

  const summary = useMemo(() => {
    const closed = visibleRows.filter((row) => row.status === "tp" || row.status === "sl");
    const tp = closed.filter((row) => row.status === "tp");
    const risks = closed.map((row) => row.riskReturn).filter((value): value is number => value !== null);
    const avgRisk = risks.length > 0 ? risks.reduce((a, b) => a + b, 0) / risks.length : null;
    return {
      closed: closed.length,
      winRate: closed.length > 0 ? (tp.length / closed.length) * 100 : null,
      avgRisk,
    };
  }, [visibleRows]);

  return <Card>
    <CardContent className="grid gap-4">
      <div className="grid gap-1">
        <h2 className="text-lg font-semibold">{title}</h2>
        <p className="text-xs text-muted-foreground">{description}</p>
      </div>
      <div className="flex flex-wrap items-end gap-3">
        {fixedDirection === null && (
          <SegmentedList role="tablist" aria-label="Направление сделки">
            <SegmentedTrigger role="tab" aria-selected={direction === "short"} onClick={() => setDirection("short")}>SHORT</SegmentedTrigger>
            <SegmentedTrigger role="tab" aria-selected={direction === "long"} onClick={() => setDirection("long")}>LONG</SegmentedTrigger>
          </SegmentedList>
        )}
        <Field label="TP, %" className="w-28">
          {(control) => <Input {...control} inputMode="decimal" value={tpInput} onChange={(event) => setTpInput(event.target.value)} />}
        </Field>
        <Field label="SL, %" className="w-28">
          {(control) => <Input {...control} inputMode="decimal" value={slInput} onChange={(event) => setSlInput(event.target.value)} />}
        </Field>
        <Field label="Time-shift, мин" className="w-32">
          {(control) => <Input {...control} inputMode="numeric" value={timeShiftInput} onChange={(event) => setTimeShiftInput(event.target.value)} />}
        </Field>
        {enableMaturity && (
          <Field label={`Зрелость: ${maturityDraft.toFixed(1)}`} className="w-52">
            {(control) => <Input
              {...control}
              type="range"
              min={0}
              max={1}
              step={0.1}
              value={maturityDraft}
              onChange={(event) => setMaturityDraft(Number(event.target.value))}
              onPointerUp={() => setMaturity(maturityDraft)}
              onKeyUp={() => setMaturity(maturityDraft)}
              onBlur={() => setMaturity(maturityDraft)}
            />}
          </Field>
        )}
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
        <Field label="С даты" className="w-40">
          {(control) => <Input {...control} type="date" value={fromDate} onChange={(event) => setFromDate(event.target.value)} />}
        </Field>
        <Field label="По дату" className="w-40">
          {(control) => <Input {...control} type="date" value={toDate} onChange={(event) => setToDate(event.target.value)} />}
        </Field>
        <Button disabled={running || setId === ""} onClick={() => { void run(); }}>{running ? "Расчёт…" : "Рассчитать"}</Button>
        {progress.total > 0 && <Badge variant={progress.done >= progress.total ? "success" : "info"}>Сделок: {progress.done} / {progress.total}</Badge>}
      </div>
      {error && <Alert variant="destructive" role="alert"><AlertDescription>{error}</AlertDescription></Alert>}
      <div className="flex flex-wrap gap-2 text-xs">
        {summary.winRate !== null && <Badge variant={summary.avgRisk !== null && summary.avgRisk >= 0 ? "success" : "danger"}>Win rate: {summary.winRate.toFixed(1)}%</Badge>}
        {summary.avgRisk !== null && <Badge>Матожидание: {formatRisk(summary.avgRisk)} по {summary.closed} закрытым</Badge>}
        {filteredCount > 0 && <Badge variant="warning">Отфильтровано смещением цены: {filteredCount}</Badge>}
      </div>
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

export function AnalyticsPage() {
  const [sets, setSets] = useState<AnalyticsSet[]>([]);
  const [setsState, setSetsState] = useState<"loading" | "ready" | "error">("loading");
  const [setId, setSetId] = useState("");

  useEffect(() => {
    let cancelled = false;
    void fetchAnalyticsSets().then((items) => {
      if (cancelled) return;
      setSets(items);
      setSetsState("ready");
      setSetId((current) => (current === "" && items.length > 0 ? (items[0]!.setId) : current));
    }).catch(() => { if (!cancelled) setSetsState("error"); });
    return () => { cancelled = true; };
  }, []);

  return <section aria-labelledby="analytics-heading" className="grid gap-4">
    <PageHeader id="analytics-heading" title="Аналитика" description="Backtest наборов сетапов из завершённых исторических запусков: матожидание TP/SL, зрелость кластеров, построчные закрытия. Расчёты читают минутные свечи через сервис данных; результаты кешируются." />
    <Card>
      <CardContent className="flex flex-wrap items-end gap-3">
        <Field label="Набор сетапов" className="w-full max-w-md">
          {(control) => (
            <Select {...control} value={setId} disabled={setsState !== "ready"} onChange={(event) => setSetId(event.target.value)}>
              {sets.length === 0 && <option value="">Нет завершённых запусков</option>}
              {sets.map((set) => <option key={set.setId} value={set.setId}>{set.label}</option>)}
            </Select>
          )}
        </Field>
        {setsState === "ready" && setId !== "" && (() => {
          const active = sets.find((set) => set.setId === setId);
          if (active === undefined) return null;
          return <div className="flex flex-wrap gap-2 text-xs">
            <Badge>Сетапов: {active.setupCount}</Badge>
            <Badge>{active.exchange} · {active.category}{active.symbol === null ? "" : ` · ${active.symbol}`}</Badge>
          </div>;
        })()}
      </CardContent>
    </Card>
    {setsState === "loading" && <Spinner label="Загрузка наборов…" />}
    {setsState === "error" && <Alert variant="destructive" role="alert">Наборы недоступны: не удалось получить список исторических запусков</Alert>}
    {setsState === "ready" && sets.length === 0 && (
      <EmptyState title="Нет наборов для аналитики" description="Аналитика работает по завершённым историческим запускам. Создайте запуск на странице «Запуски» и дождитесь его завершения." />
    )}
    {setsState === "ready" && sets.length > 0 && setId !== "" && <>
      <MatrixSection
        setId={setId}
        title="Матожидание по зрелости сетапа и месяцам"
        description="SHORT-матрица TP×SL по сетапам, выбранным из кластеров по зрелости. Месяц ограничивает исходные сигналы до кластеризации; горизонт сделки концом месяца не обрезается."
        direction="short"
        tpLevels={SHORT_MATRIX_LEVELS}
        slLevels={SHORT_MATRIX_LEVELS}
        enableMaturity
        enableMonth
        enablePriceChange
      />
      <TradesSection
        setId={setId}
        title="SHORT: сделки по сетапам"
        description="Построчный список SHORT-сделок по сетапам набора: вход по close свечи через time-shift, закрытие по TP или SL за 7 дней. Сначала показываются кешированные строки, недостающие досчитываются."
        fixedDirection="short"
        enableMaturity
        enablePriceChange
      />
      <MatrixSection
        setId={setId}
        title="Матожидание за всё время — LONG"
        description="LONG-матрица TP×SL на всех сетапах набора или выбранного месяца. Гейт смещения цены зеркален: LONG заходит только при падении цены на заданный процент."
        direction="long"
        tpLevels={LONG_TP_LEVELS}
        slLevels={LONG_SL_LEVELS}
        enableMaturity={false}
        enableMonth
        enablePriceChange
      />
      <TradesSection
        setId={setId}
        title="Сетапы: уровни закрытия"
        description="Построчные закрытия с переключателем направления. Для каждой строки показывается цена входа, уровень TP/SL, время касания и результат в рисках."
        fixedDirection={null}
        enableMaturity={false}
        enablePriceChange={false}
      />
      <AnalyticsCacheCard />
    </>}
  </section>;
}
