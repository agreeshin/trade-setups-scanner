import { useEffect, useState, type FormEvent } from "react";
import { Alert } from "../components/ui/alert";
import { Button } from "../components/ui/button";
import { Checkbox } from "../components/ui/checkbox";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "../components/ui/dialog";
import { Field } from "../components/ui/field";
import { Input } from "../components/ui/input";
import { Select } from "../components/ui/select";
import { Spinner } from "../components/ui/spinner";
import { fetchPlatform } from "../lib/platform-api";
import { isActiveStudy, type OptimizationStudyAction } from "../lib/optimization-api";
import { InstrumentPicker, type CatalogInstrument } from "./instrument-picker";
import { errorText, localDateInput, type Algorithm } from "./platform-controls";
import { ANALYTICS_BASELINE, ANALYTICS_SEARCH_FIELDS, buildOptimizationSpec, DEFAULT_AXIS, initialDomains, type DomainDrafts, type ScopeDraft, type SearchField } from "./optimization-form";
import { OptimizationResults } from "./optimization-results";
import { OptimizationTrashIcon } from "./optimization-history";
import { useOptimizationDetail } from "./use-optimization-detail";
import type { OptimizationController } from "./use-optimization-studies";

function DomainControls({ fields, value, onChange }: { fields: SearchField[]; value: DomainDrafts; onChange: (next: DomainDrafts) => void }) {
  return <div className="grid gap-3">{fields.map((field) => {
    const draft = value[field.key]!;
    const update = (patch: Partial<typeof draft>) => onChange({ ...value, [field.key]: { ...draft, ...patch } });
    return <fieldset key={field.key} className="grid min-w-0 gap-3 rounded-md border border-input p-3 sm:grid-cols-2">
      <legend className="px-1 font-medium">{field.label}{field.unit ? ` (${field.unit})` : ""}</legend>
      <Field label={`Baseline: ${field.label}`} hint={field.type === "boolean" ? "true / false" : field.type === "nullable" ? "null — гейт отключён; 0 — отдельный числовой порог" : `${field.min}..${field.max}${field.type === "integer" ? ", целое" : ", десятичная точка"}`}>
        {(control) => field.type === "boolean" ? <Select {...control} value={draft.baseline} onChange={(event) => update({ baseline: event.target.value })}><option value="true">Включена</option><option value="false">Выключена</option></Select>
          : <Input {...control} maxLength={128} value={draft.baseline} onChange={(event) => update({ baseline: event.target.value })} />}
      </Field>
      <div className="grid gap-2">
        <label className="flex items-center gap-2 text-sm"><Checkbox checked={draft.search} onCheckedChange={(checked) => update({ search: checked === true })} />Подбирать: {field.label}</label>
        <Field label={`Значения: ${field.label}`} hint="Через запятую, до 256 значений. Выключенный подбор фиксирует baseline.">
          {(control) => <Input {...control} disabled={!draft.search} maxLength={8192} value={draft.values} onChange={(event) => update({ values: event.target.value })} />}
        </Field>
      </div>
    </fieldset>;
  })}</div>;
}

function OptimizerForm({ algorithm, controller, onStarted }: { algorithm: Algorithm; controller: OptimizationController; onStarted: () => void }) {
  const [algorithmDomains, setAlgorithmDomains] = useState(() => initialDomains(algorithm.fields, algorithm.params));
  const [analyticsDomains, setAnalyticsDomains] = useState(() => initialDomains(ANALYTICS_SEARCH_FIELDS, { ...ANALYTICS_BASELINE }));
  const [scope, setScope] = useState<ScopeDraft>({ from: "", to: "", symbols: "", direction: "short", setupMaturity: "0", horizonMinutes: "10080", tpLevels: DEFAULT_AXIS, slLevels: DEFAULT_AXIS, minClosedPerCell: "1", minSetupsPerDay: "4", seed: "42", maxTrials: "256", maxSeconds: "3600" });
  const [catalog, setCatalog] = useState<CatalogInstrument[]>([]);
  const [ready, setReady] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [revision, setRevision] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const browserTimezone = Intl.DateTimeFormat().resolvedOptions().timeZone;
  useEffect(() => {
    const controller = new AbortController();
    void Promise.all([
      fetchPlatform<{ instruments: CatalogInstrument[] }>("/v1/instruments", controller.signal),
      fetchPlatform<{ value: { replay?: { symbols?: string; instruments?: { symbol: string; exchange: string; category: string }[]; fromUnix?: number; toUnix?: number; from?: string; to?: string } } }>("/v1/settings/ui", controller.signal),
    ]).then(([instruments, preferences]) => {
      if (controller.signal.aborted) return;
      if (instruments.kind !== "ready" || preferences.kind !== "ready") {
        setLoadError("Не удалось загрузить historical-каталог или сохранённый выбор. Запуск заблокирован; повторите загрузку.");
        return;
      }
      const items = instruments.data.instruments.filter((item) => item.exchange === "bybit" && item.category === "linear");
      const saved = preferences.data.value.replay;
      setCatalog(items);
      setScope((previous) => ({ ...previous,
        symbols: typeof saved?.symbols === "string" ? saved.symbols : (saved?.instruments ?? items).filter((item) => item.exchange === "bybit" && item.category === "linear").map((item) => item.symbol).join("\n"),
        from: typeof saved?.fromUnix === "number" ? localDateInput(saved.fromUnix) : saved?.from ?? "",
        to: typeof saved?.toUnix === "number" ? localDateInput(saved.toUnix) : saved?.to ?? "",
      }));
      setLoadError(null); setReady(true);
    });
    return () => controller.abort();
  }, [revision]);
  const change = (patch: Partial<ScopeDraft>) => { setScope({ ...scope, ...patch }); setError(null); };
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!ready || controller.creating || controller.active || !controller.ready) return;
    setError(null);
    try {
      const spec = buildOptimizationSpec(algorithm, scope, catalog, algorithmDomains, analyticsDomains);
      if (await controller.start(spec)) onStarted();
    } catch (cause) { setError(errorText(cause)); }
  }
  const numericFields: { key: "setupMaturity" | "horizonMinutes" | "minClosedPerCell" | "minSetupsPerDay" | "seed" | "maxTrials" | "maxSeconds"; label: string; min: number; max?: number; step?: number; hint?: string }[] = [
    { key: "setupMaturity", label: "Зрелость (фиксирована)", min: 0, max: 1, step: 0.1 },
    { key: "horizonMinutes", label: "Горизонт сделки, мин (фиксирован)", min: 1, max: 43200 },
    { key: "minClosedPerCell", label: "Минимум закрытых на ячейку", min: 1 },
    { key: "minSetupsPerDay", label: "Минимум сетапов в день", min: 0, step: 0.1, hint: "Среднее по всем выбранным инструментам за весь точный выбранный период сигналов, включая дробные дни и интервалы без сигналов. 0 отключает только частотный гейт." },
    { key: "seed", label: "Seed", min: 0 }, { key: "maxTrials", label: "Бюджет trials", min: 1, max: 4096 },
    { key: "maxSeconds", label: "Бюджет времени, секунд", min: 1, max: 86400 },
  ];
  return <form noValidate onSubmit={(event) => { void submit(event); }} className="grid min-w-0 gap-5">
    <Alert>Исследование изолировано от live, обычных прогонов и кешей. Все выбранные TP×SL-ячейки имеют равный вес; baseline оценивается как trial 0 и входит в бюджет. Открытие и изменение формы ничего не запускают.</Alert>
    {!ready && !loadError && <Spinner label="Загрузка historical-списка" />}
    {loadError && <Alert variant="destructive" role="alert">{loadError}<Button type="button" variant="outline" onClick={() => setRevision((value) => value + 1)}>Повторить загрузку</Button></Alert>}
    <fieldset disabled={controller.creating} className="grid min-w-0 gap-4">
      <legend className="mb-2 font-semibold">Фиксированный scope</legend>
      <p className="text-xs text-muted-foreground">Часовой пояс ввода: {browserTimezone} (часовой пояс браузера). Период по времени закрытия сигнала [начало, конец), границы кратны минуте. Горизонт сделки не обрезается концом периода.</p>
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Начало периода сигналов">{(control) => <Input {...control} type="datetime-local" step={60} value={scope.from} onChange={(event) => change({ from: event.target.value })} />}</Field>
        <Field label="Конец периода сигналов">{(control) => <Input {...control} type="datetime-local" step={60} value={scope.to} onChange={(event) => change({ to: event.target.value })} />}</Field>
        <Field label="Направление сделки">{(control) => <Select {...control} value={scope.direction} onChange={(event) => change({ direction: event.target.value === "long" ? "long" : "short" })}><option value="short">SHORT</option><option value="long">LONG</option></Select>}</Field>
      </div>
      <InstrumentPicker catalog={catalog} symbols={scope.symbols} onChange={(symbols) => change({ symbols })} disabled={!ready || controller.creating} limit={null} />
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Уровни TP, %" hint="1..64 уникальных значений через запятую; 0 < TP ≤ 100.">{(control) => <Input {...control} maxLength={8192} value={scope.tpLevels} onChange={(event) => change({ tpLevels: event.target.value })} />}</Field>
        <Field label="Уровни SL, %" hint="Независимая ось: 0 < SL ≤ 100, не более 64 уровней.">{(control) => <Input {...control} maxLength={8192} value={scope.slLevels} onChange={(event) => change({ slLevels: event.target.value })} />}</Field>
        {numericFields.map((field) => <Field key={field.key} label={field.label} hint={field.hint}>{(control) => <Input {...control} type="number" min={field.min} max={field.max} step={field.step ?? 1} value={scope[field.key]} onChange={(event) => change({ [field.key]: event.target.value })} />}</Field>)}
      </div>
      <p className="text-xs text-muted-foreground">Выбрано ячеек TP×SL: {scope.tpLevels.split(",").filter((item) => item.trim()).length} × {scope.slLevels.split(",").filter((item) => item.trim()).length}. Зрелость не подбирается; без кластеризации не применяется. Гейт null отличается от порога 0.</p>
      <h3 className="font-semibold">Параметры алгоритма · {algorithm.version}</h3>
      <p className="text-xs text-muted-foreground">Baseline скопирован из сохранённой конфигурации, а не из несохранённых правок карточки. Изменения здесь относятся только к исследованию. Составные ограничения проверяет сервер.</p>
      <DomainControls fields={algorithm.fields} value={algorithmDomains} onChange={setAlgorithmDomains} />
      {Object.entries(algorithm.params).filter(([key]) => !algorithm.fields.some((field) => field.key === key)).map(([key, value]) => <p key={key} className="text-sm">{key}: {value} (фиксирован)</p>)}
      <h3 className="font-semibold">Параметры аналитики</h3>
      <DomainControls fields={ANALYTICS_SEARCH_FIELDS} value={analyticsDomains} onChange={setAnalyticsDomains} />
    </fieldset>
    {error && <Alert variant="destructive" role="alert">{error}</Alert>}
    <div><Button type="submit" disabled={!ready || !controller.ready || controller.creating || controller.active !== null}>{controller.creating ? "Создание исследования…" : "Начать исследование"}</Button></div>
  </form>;
}

function StudyDetail({ detail, algorithm, controller, onDelete, onNew }: {
  detail: ReturnType<typeof useOptimizationDetail>; algorithm: Algorithm; controller: OptimizationController;
  onDelete: (study: OptimizationStudyAction) => void; onNew?: () => void;
}) {
  return <>
    {detail?.error && <Alert variant="destructive" role="alert">{detail.error}<Button type="button" variant="outline" onClick={controller.refresh}>Повторить загрузку результата</Button></Alert>}
    {!detail?.study && !detail?.error && <Spinner label="Загрузка исследования" />}
    {detail?.study && <>
      <OptimizationResults study={detail.study} algorithm={algorithm} controller={controller} {...(onNew ? { onNew } : {})} />
      {!isActiveStudy(detail.study) && <div><Button type="button" variant="outline" disabled={controller.creating || controller.cancelling !== null || controller.deleting !== null} aria-label={`Удалить исследование ${detail.study.id}`} onClick={() => onDelete(detail.study!)}><OptimizationTrashIcon />Удалить исследование</Button></div>}
    </>}
  </>;
}

// A history selection has its own sibling dialog/state. It never follows "latest"
// and closes/reopens the exact ID even when discovery finds a newer study.
export function OptimizationStudyDialog({ id, algorithm, controller, open, onOpenChange, returnFocusId, onDelete }: {
  id: number | undefined; algorithm: Algorithm; controller: OptimizationController; open: boolean;
  onOpenChange: (open: boolean) => void; returnFocusId: string; onDelete: (study: OptimizationStudyAction) => void;
}) {
  const detail = useOptimizationDetail(id, open, controller.revision);
  return <Dialog open={open} onOpenChange={onOpenChange}>
    <DialogContent className="max-w-4xl" onCloseAutoFocus={(event) => { event.preventDefault(); (document.getElementById(`optimization-study-${id}`) ?? document.getElementById(returnFocusId))?.focus(); }}>
      <DialogHeader><DialogTitle>Исследование #{id} · {algorithm.name}</DialogTitle><DialogDescription>Сохранённое исследование. Просмотр и закрытие окна не меняют серверную работу или настройки.</DialogDescription></DialogHeader>
      <div className="grid min-h-0 min-w-0 flex-1 gap-4 overflow-y-auto pr-1">
        {controller.error && <Alert variant="destructive" role="alert">{controller.error}</Alert>}
        <StudyDetail detail={detail} algorithm={algorithm} controller={controller} onDelete={onDelete} />
      </div>
    </DialogContent>
  </Dialog>;
}

export function AlgorithmOptimizer({ algorithm, controller, open, onOpenChange, returnFocusId, onDelete }: { algorithm: Algorithm; controller: OptimizationController; open: boolean; onOpenChange: (open: boolean) => void; returnFocusId: string; onDelete: (study: OptimizationStudyAction) => void }) {
  const study = controller.byAlgorithm[algorithm.id];
  const [drafting, setDrafting] = useState(false);
  const showStudy = study && (isActiveStudy(study) || !drafting);
  const detail = useOptimizationDetail(study?.id, open && Boolean(showStudy), controller.revision, study && !isActiveStudy(study) ? study.updatedAt : undefined);
  return <Dialog open={open} onOpenChange={(next) => { if (!next) setDrafting(false); onOpenChange(next); }}>
    <DialogContent className="max-w-4xl" onCloseAutoFocus={(event) => { event.preventDefault(); document.getElementById(returnFocusId)?.focus(); }}>
      <DialogHeader>
        <DialogTitle>Оптимизация · {algorithm.name}</DialogTitle>
        <DialogDescription>Серверное исследование Plan A. Закрытие окна не останавливает работу; для остановки нужна отдельная команда.</DialogDescription>
      </DialogHeader>
      <div className="grid min-h-0 min-w-0 flex-1 gap-4 overflow-y-auto pr-1">
        {controller.error && <Alert variant="destructive" role="alert">{controller.error}</Alert>}
        {!controller.ready && <div className="flex flex-wrap items-center gap-2 text-sm"><span>Проверяем сохранённые исследования. Новый запуск пока недоступен.</span><Button type="button" variant="outline" onClick={controller.refresh}>Обновить статус</Button></div>}
        {controller.active && controller.active.spec.algorithmId !== algorithm.id && <Alert variant="warning">Исследование #{controller.active.id} для {controller.active.spec.algorithmId} уже выполняется. В платформе допускается только одно активное исследование.</Alert>}
        {showStudy ? <StudyDetail detail={detail} algorithm={algorithm} controller={controller} onDelete={onDelete} onNew={() => setDrafting(true)} />
          : <OptimizerForm algorithm={algorithm} controller={controller} onStarted={() => setDrafting(false)} />}
      </div>
    </DialogContent>
  </Dialog>;
}
