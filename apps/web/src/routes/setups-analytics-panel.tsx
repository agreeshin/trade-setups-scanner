import { useEffect, useMemo, useRef, useState } from "react";
import { Alert, AlertDescription } from "../components/ui/alert";
import { Badge } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import { Card, CardContent } from "../components/ui/card";
import { Checkbox } from "../components/ui/checkbox";
import { DateInput } from "../components/ui/date-input";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "../components/ui/dialog";
import { EmptyState } from "../components/ui/empty-state";
import { Field } from "../components/ui/field";
import { InfoNote } from "../components/ui/info-note";
import { Select } from "../components/ui/select";
import { Spinner } from "../components/ui/spinner";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "../components/ui/table";
import { fetchPlatform } from "../lib/platform-api";
import { getOptimizationStudy, type OptimizationMatrixCell, type OptimizationStudy } from "../lib/optimization-api";
import {
  fetchExpectancyPngUrl,
  fetchSetupsAnalytics,
  fetchSetupsAnalyticsStudies,
  type SetupsAnalyticsResult,
  type SetupsAnalyticsSetup,
  type SetupsAnalyticsStatus,
  type SetupsAnalyticsStudyMeta,
} from "../lib/setups-analytics-api";
import {
  algorithmDisplayName,
  errorText,
  localDateInput,
  localTime,
  localTimezoneLabel,
  replayRange,
  utcMidnightOnDateChange,
  type Algorithm,
} from "./platform-controls";

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

function formatPrice(value: number | null): string {
  if (value === null) return "—";
  return String(Number(value.toPrecision(6)));
}

function formatRisk(value: number | null): string {
  if (value === null) return "—";
  return `${value >= 0 ? "+" : ""}${value.toFixed(2)}R`;
}

function formatSignedR(value: number): string {
  return `${value >= 0 ? "+" : ""}${value.toFixed(2)} R`;
}

function formatTime(unix: number | null): string {
  if (unix === null) return "—";
  return localTime(unix);
}

function meanColorClass(value: number | null): string {
  if (value === null || value === 0) return "text-muted-foreground";
  return value > 0 ? "text-success" : "text-danger";
}

const STATUS_BADGES: Record<SetupsAnalyticsStatus, { label: string; variant: "success" | "danger" | "neutral" | "warning" }> = {
  tp: { label: "TP", variant: "success" },
  sl: { label: "SL", variant: "danger" },
  unclosed: { label: "не закрылась", variant: "neutral" },
  no_data: { label: "нет данных", variant: "neutral" },
  gate_failed: { label: "гейт не пройден", variant: "warning" },
};

function GateBadge({ passed, selected }: { passed: number; selected: number }) {
  return <Badge className="bg-ambient-violet/15 text-ambient-violet" aria-label={`Прошли гейт: ${passed} из ${selected}`}>{passed}/{selected}</Badge>;
}

function SetupOutcomeRow({ setup }: { setup: SetupsAnalyticsSetup }) {
  const status = STATUS_BADGES[setup.status];
  return <TableRow>
    <TableCell className="font-medium">{setup.symbol}</TableCell>
    <TableCell className="whitespace-nowrap tabular-nums">{formatTime(setup.setupTimeUnix)}</TableCell>
    <TableCell numeric>{formatPrice(setup.entryPrice)}</TableCell>
    <TableCell numeric className="text-success">{formatPrice(setup.tpPrice)}</TableCell>
    <TableCell numeric className="text-danger">{formatPrice(setup.slPrice)}</TableCell>
    <TableCell className="whitespace-nowrap tabular-nums">{formatTime(setup.closeTimeUnix)}</TableCell>
    <TableCell numeric className={(setup.riskReturn ?? 0) >= 0 ? "text-success" : "text-danger"}>{formatRisk(setup.riskReturn)}</TableCell>
    <TableCell><Badge variant={status.variant}>{status.label}</Badge></TableCell>
    <TableCell numeric>№{setup.slotId}</TableCell>
  </TableRow>;
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

function StudyDetailsDialog({ studyId, meta, onClose }: { studyId: number | null; meta: SetupsAnalyticsStudyMeta | undefined; onClose: () => void }) {
  const [study, setStudy] = useState<OptimizationStudy | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [pngUrl, setPngUrl] = useState<string | null>(null);
  const [pngError, setPngError] = useState<string | null>(null);

  useEffect(() => {
    if (studyId === null) return;
    const controller = new AbortController();
    void getOptimizationStudy(studyId, controller.signal)
      .then((value) => { if (!controller.signal.aborted) setStudy(value); })
      .catch((cause) => { if (!controller.signal.aborted) setLoadError(errorText(cause)); });
    const artifactId = meta?.artifactId;
    if (artifactId !== undefined && artifactId !== null) {
      void fetchExpectancyPngUrl(artifactId, controller.signal)
        .then((url) => { if (!controller.signal.aborted) setPngUrl(url); })
        .catch((cause) => { if (!controller.signal.aborted) setPngError(errorText(cause)); });
    }
    return () => controller.abort();
  }, [studyId, meta]);

  useEffect(() => () => { if (pngUrl !== null) URL.revokeObjectURL(pngUrl); }, [pngUrl]);

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
    {studyId !== null && <DialogContent className="max-w-2xl" aria-label={`Исследование №${studyId}`}>
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
            : <dl className="mt-2 grid gap-2 text-sm sm:grid-cols-2">{Object.entries(best.params).map(([key, value]) => <div key={key} className="break-words"><dt className="text-muted-foreground">{key}</dt><dd>{String(value)}</dd></div>)}</dl>}
        </div>
      </div>}
      <div className="grid gap-2">
        <h4 className="font-medium">Динамика матожидания</h4>
        {pngUrl !== null && <img src={pngUrl} alt={`Динамика матожидания исследования №${studyId}`} className="w-full rounded-md border border-input/60" />}
        {pngUrl === null && pngError !== null && <p className="text-sm text-muted-foreground">{pngError}</p>}
        {pngUrl === null && pngError === null && (meta?.artifactId === undefined || meta.artifactId === null) && <p className="text-sm text-muted-foreground">PNG недоступен: у сетапов этого исследования нет сохранённого графика.</p>}
        {pngUrl === null && pngError === null && meta?.artifactId != null && <p className="text-sm text-muted-foreground">Загрузка графика…</p>}
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
  const [result, setResult] = useState<SetupsAnalyticsResult | null>(null);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const calculateRef = useRef<AbortController | null>(null);
  const restoredSelection = useRef<ReadonlySet<number> | null>(initialSelectedStudies());

  const range = useMemo(() => {
    try {
      return { value: replayRange(fromInput, toInput), error: null as string | null };
    } catch (cause) {
      return { value: null, error: errorText(cause) };
    }
  }, [fromInput, toInput]);
  const committedRange = useMemo(() => {
    try {
      return replayRange(committed.from, committed.to);
    } catch {
      return null;
    }
  }, [committed]);
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
      const data = await fetchSetupsAnalytics(algorithmId, range.value.fromUnix, range.value.toUnix, studies, controller.signal);
      if (!controller.signal.aborted) setResult(data);
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
      <InfoNote>Разовый расчёт по сохранённым рекомендациям исследований: вход по рекомендованному времени и гейту смещения цены, исход TP/SL по реальным минутным свечам за 7 суток. Результаты не кешируются.</InfoNote>
    </div>
    <Card>
      <CardContent className="flex flex-wrap items-end gap-3">
        <Field label="Алгоритм" className="w-full max-w-xs">
          {(control) => <Select {...control} value={algorithmId} disabled={algorithms === null} onChange={(event) => { setAlgorithmId(event.target.value); setResult(null); }}>
            {(algorithms ?? []).map((algorithm) => <option key={algorithm.id} value={algorithm.id}>{algorithmDisplayName(algorithm)}</option>)}
          </Select>}
        </Field>
        <Field label="Начало (момент выхода сетапа)" className="w-60">
          {(control) => <DateInput {...control} type="datetime-local" value={fromInput} onBlur={commitDates} onChange={(event) => { setFromInput(utcMidnightOnDateChange(fromInput, event.target.value)); setResult(null); }} />}
        </Field>
        <Field label="Конец" className="w-60">
          {(control) => <DateInput {...control} type="datetime-local" value={toInput} onBlur={commitDates} onChange={(event) => { setToInput(utcMidnightOnDateChange(toInput, event.target.value)); setResult(null); }} />}
        </Field>
        <Button variant="outline" disabled={studiesMeta.length === 0} onClick={openPicker}>Исследования: {studiesLabel}</Button>
        <Button disabled={running || algorithmId === "" || range.value === null} onClick={() => { void run(); }}>{running ? "Расчёт…" : "Рассчитать"}</Button>
        {studiesStatus === "loading" && <Spinner label="Загрузка исследований…" />}
        <p className="w-full text-xs text-muted-foreground">Время: {localTimezoneLabel()}. Смена даты сбрасывает время к 00:00 UTC; список исследований обновляется при потере фокуса полей дат, после обновления выбраны все. Расчёт выполняется по нажатию кнопки, без кеша.</p>
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
                    <button type="button" className="font-medium text-primary underline-offset-4 hover:underline" aria-label={`Подробности исследования ${study.studyId}`} onClick={() => setDetailStudyId(study.studyId)}>№{study.studyId}</button>
                  </TableCell>
                  <TableCell><GateBadge passed={study.gatePassed} selected={study.selected} /></TableCell>
                  <TableCell numeric className={`font-medium ${meanColorClass(study.meanR)}`}>{formatRisk(study.meanR)}</TableCell>
                  <TableCell numeric>{study.closed}</TableCell>
                </TableRow>)}
              </TableBody>
            </Table>
          </div>
        </div>}
        {result.setups.length === 0
          ? <EmptyState title="Нет сетапов по выбранным фильтрам" description="Измените диапазон, алгоритм или набор исследований." />
          : <div className="grid gap-2">
            <h3 className="text-sm font-semibold text-muted-foreground">Сетапы</h3>
            <div className="overflow-x-auto rounded-md border border-input/60">
              <Table>
                <caption className="px-3 py-2 text-left text-xs text-muted-foreground">Время: {localTimezoneLabel()}</caption>
                <TableHeader>
                  <TableRow className="hover:bg-transparent">
                    <TableHead>Инструмент</TableHead>
                    <TableHead>Время сетапа</TableHead>
                    <TableHead numeric>Вход</TableHead>
                    <TableHead numeric className="text-success">TP</TableHead>
                    <TableHead numeric className="text-danger">SL</TableHead>
                    <TableHead>Закрытие</TableHead>
                    <TableHead numeric>Результат</TableHead>
                    <TableHead>Статус</TableHead>
                    <TableHead numeric>Профиль</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {result.setups.map((setup) => <SetupOutcomeRow key={setup.id} setup={setup} />)}
                </TableBody>
              </Table>
            </div>
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
    <StudyDetailsDialog key={detailStudyId ?? "none"} studyId={detailStudyId} meta={detailMeta} onClose={() => setDetailStudyId(null)} />
  </section>;
}
