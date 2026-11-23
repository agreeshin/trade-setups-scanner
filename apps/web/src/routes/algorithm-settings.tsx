import { useCallback, useEffect, useId, useMemo, useRef, useState, type FormEvent } from "react";
import { Alert } from "../components/ui/alert";
import { Button } from "../components/ui/button";
import { StatusDot } from "../components/ui/badge";
import { Checkbox } from "../components/ui/checkbox";
import { Field } from "../components/ui/field";
import { Input } from "../components/ui/input";
import { PageSidebar, PageSidebarPanel } from "../components/ui/page-sidebar";
import { InfoNote } from "../components/ui/info-note";
import { Separator } from "../components/ui/separator";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "../components/ui/tabs";
import { fetchPlatform, type PlatformState } from "../lib/platform-api";
import type { OptimizationStudyAction } from "../lib/optimization-api";
import { isContinuousWorkActive } from "../lib/continuous-api";
import { algorithmDisplayName, errorText, writePlatform, type Algorithm } from "./platform-controls";
import { AlgorithmOptimizer, OptimizationStudyDialog } from "./algorithm-optimizer";
import { OptimizationDeleteDialog, OptimizationHistory } from "./optimization-history";
import { useOptimizationStudies, type OptimizationController } from "./use-optimization-studies";
import { useContinuousStudy } from "./use-continuous-study";
import { useAdaptiveProfiles } from "./use-adaptive-profiles";
import { AdaptiveLive, AdaptiveProfiles, ContinuousControls, ContinuousResearch } from "./continuous-study";
import { ContinuousHistoryCleanup } from "./continuous-history-cleanup";

const groups = [
  { title: "Цена и объём", keys: ["priceGrowthPct", "priceDropPct", "windowMinutes", "volumeMultiplier"] },
  { title: "Базовый объём", keys: ["baselineLongDays", "baselineShortDays"] },
  { title: "Качество данных и повторные сигналы", keys: ["cooldownMinutes", "maxGapPercent", "maxConsecutiveGapMinutes", "requiredRecentMinutes"] },
];

type SavedAlgorithm = Partial<Pick<Algorithm, "enabled" | "externalName">> & { adaptiveEnabled?: boolean };
function AlgorithmForm({ algorithm, optimization, onSaved, onActivity, continuousRevision, cleanupRevision, selected, onCleanup, refreshStates }: { algorithm: Algorithm; optimization: OptimizationController; onSaved: (id: string, patch: SavedAlgorithm) => void; onActivity: (id: string, active: boolean) => void; continuousRevision: number; cleanupRevision: number; selected: boolean; onCleanup: () => void; refreshStates: () => void }) {
  const [liveMode, setLiveMode] = useState("flexible");
  const continuous = useContinuousStudy(algorithm.id, continuousRevision, selected && liveMode === "flexible");
  const profiles = useAdaptiveProfiles(algorithm.id, selected && liveMode === "flexible", continuousRevision);
  const displayName = algorithmDisplayName(algorithm);
  const [externalName, setExternalName] = useState(displayName);
  const [nameBusy, setNameBusy] = useState(false);
  const [nameError, setNameError] = useState<string | null>(null);
  const [nameSaved, setNameSaved] = useState(false);
  const nameRequest = useRef(false);
  const { refresh: refreshOptimization } = optimization;
  const adaptiveSaved = profiles.available ? profiles.permission ?? undefined : undefined;
  const continuousActive = isContinuousWorkActive(continuous.process);
  useEffect(() => { onActivity(algorithm.id, continuousActive); }, [algorithm.id, continuousActive, onActivity]);
  useEffect(() => { if (adaptiveSaved !== undefined) onSaved(algorithm.id, { adaptiveEnabled: adaptiveSaved }); }, [adaptiveSaved, algorithm.id, onSaved]);
  useEffect(() => { if (continuous.revision > 0) refreshOptimization(); }, [continuous.revision, refreshOptimization]);
  const [params, setParams] = useState(algorithm.params);
  const [baseline, setBaseline] = useState(algorithm.params);
  const optimizerAlgorithm = useMemo(() => ({ ...algorithm, params: baseline }), [algorithm, baseline]);
  const [optimizationOpen, setOptimizationOpen] = useState(false);
  const [optimizationIntent, setOptimizationIntent] = useState(0);
  const [historySelection, setHistorySelection] = useState<{ id: number; open: boolean; triggerId: string; continuous: boolean; cleanupRevision: number } | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<(OptimizationStudyAction & { cleanupRevision: number }) | null>(null);
  const selectedHistory = historySelection?.continuous && historySelection.cleanupRevision !== cleanupRevision ? null : historySelection;
  const selectedDeleteTarget = deleteTarget?.origin === "continuous" && deleteTarget.cleanupRevision !== cleanupRevision ? null : deleteTarget;
  const optimizerTriggerId = useId();
  const historyRegionId = useId();
  const [enabled, setEnabled] = useState(algorithm.enabled);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const ungrouped = algorithm.fields.filter((field) => !groups.some((group) => group.keys.includes(field.key)));
  const fieldGroups = [...groups, ...(ungrouped.length ? [{ title: "Другие параметры", keys: ungrouped.map((field) => field.key) }] : [])];
  const requestDelete = (study: OptimizationStudyAction) => {
    optimization.clearError(); setDeleteTarget({ id: study.id, status: study.status, ...(study.origin ? { origin: study.origin } : {}), cleanupRevision });
  };
  const selectHistory = (id: number, triggerId: string, continuous: boolean) => {
    setOptimizationOpen(false); setHistorySelection({ id, open: true, triggerId, continuous, cleanupRevision });
  };
  async function save(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setError(null);
    setSaved(false);
    const values = { ...params };
    for (const field of algorithm.fields) {
      const raw = String(params[field.key] ?? "");
      const number = Number(raw);
      if (!raw.trim() || !Number.isFinite(number) || number < field.min || number > field.max || (field.type === "integer" && !Number.isInteger(number))) {
        setError(`Проверьте поле «${field.label}»: от ${field.min} до ${field.max} ${field.unit}.`);
        return;
      }
      values[field.key] = field.type === "integer" ? number : raw;
    }
    setBusy(true);
    try {
      await writePlatform(`/v1/algorithms/${encodeURIComponent(algorithm.id)}`, { params: values, enabled });
      setBaseline(values);
      onSaved(algorithm.id, { enabled });
      setSaved(true);
    } catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }
  async function saveName() {
    if (nameRequest.current) return;
    const name = externalName.trim();
    setNameError(null); setNameSaved(false);
    if ([...name].length < 1 || [...name].length > 120 || /\p{Cc}/u.test(name)) { setNameError("Название: от 1 до 120 символов, без управляющих символов."); return; }
    if (name === displayName) { setExternalName(name); return; }
    nameRequest.current = true;
    setNameBusy(true);
    try {
      // A rename is its own partial update. Never resend stale params/enabled with it.
      await writePlatform(`/v1/algorithms/${encodeURIComponent(algorithm.id)}`, { externalName: name });
      setExternalName(name); onSaved(algorithm.id, { externalName: name }); setNameSaved(true);
    } catch (cause) { setNameError(errorText(cause)); }
    finally { nameRequest.current = false; setNameBusy(false); }
  }
  const liveTabs = [
    { id: "flexible", label: "Live-сканирование с гибкими параметрами", enabled: adaptiveSaved === true },
    { id: "constant", label: "Live-сканирование с постоянными параметрами", enabled: algorithm.enabled },
  ];
  return <><form onSubmit={(event) => { void save(event); }} className="grid min-w-0 gap-4">
    <div className="flex min-w-0 items-center justify-center gap-2 text-center"><h3 className="min-w-0 text-lg font-semibold">{displayName} · {algorithm.version}</h3><InfoNote label="Описание алгоритма">{algorithm.description}</InfoNote></div>
    <Field label="Внешнее название алгоритма" hint="До 120 символов. Сохраняется при потере фокуса. Отдельная пользовательская подпись; служебное имя, ID, версия и старые сетапы не меняются.">{(control) => <Input {...control} maxLength={240} disabled={nameBusy} value={externalName} onChange={(event) => { setExternalName(event.target.value); setNameSaved(false); setNameError(null); }} onBlur={() => { void saveName(); }} onKeyDown={(event) => { if (event.key === "Enter") { event.preventDefault(); event.stopPropagation(); event.currentTarget.blur(); } }} />}</Field>
    {nameBusy && <p role="status" className="text-sm">Сохранение названия…</p>}
    {nameError && <Alert variant="destructive" role="alert">{nameError}</Alert>}
    {nameSaved && <p role="status" className="text-sm">Название алгоритма сохранено.</p>}
    <Tabs value={liveMode} onValueChange={setLiveMode} className="grid min-w-0 gap-4">
    <TabsList aria-label={`Режим live · ${displayName}`} className="h-auto w-full min-w-0 justify-start gap-0 rounded-none border-0 border-b bg-transparent p-0 sm:w-fit sm:justify-center-safe sm:justify-self-center">
      {liveTabs.map((tab) => <TabsTrigger key={tab.id} value={tab.id} aria-label={tab.label}
        aria-describedby={tab.enabled ? `${optimizerTriggerId}-status-${tab.id}` : undefined}
        className="h-auto gap-2 rounded-none border-b-2 border-transparent px-4 py-3 focus-visible:ring-inset data-[state=active]:border-primary data-[state=active]:bg-transparent data-[state=active]:text-primary data-[state=active]:shadow-none">
        {tab.label}{tab.enabled && <><StatusDot active data-slot="tabs-status-dot" /><span id={`${optimizerTriggerId}-status-${tab.id}`} className="sr-only">Live-сканирование включено</span></>}
      </TabsTrigger>)}
    </TabsList>
    <TabsContent value="constant" forceMount hidden={liveMode !== "constant"} className="mt-0 min-w-0">
    <section className="grid min-w-0 gap-4">
    <div className="flex items-center justify-center gap-2 text-center"><h4 className="font-semibold">Live-сканирование с постоянными параметрами</h4>
    <InfoNote>Порог цены задаётся в процентах — рост или падение, зависит от алгоритма; окно в минутах. Множитель объёма 3 означает 3× (300% базового объёма). Изменения общие для live и новых исторических запусков; завершённые запуски сохраняют свой снимок параметров.</InfoNote></div>
    <label className="flex items-center gap-2 text-sm"><Checkbox checked={enabled} onCheckedChange={(checked) => { setEnabled(checked === true); setSaved(false); }} />Включён для live-сканирования</label>
    {fieldGroups.map((group) => <fieldset key={group.title} className="grid gap-3 sm:grid-cols-2">
      <legend className="mb-2 font-medium">{group.title}</legend>
      {algorithm.fields.filter((field) => group.keys.includes(field.key)).map((field) => <Field key={field.key} label={`${field.label} (${field.unit})`}>
        {(control) => <Input type="number" required min={field.min} max={field.max} step={field.type === "integer" ? "1" : "any"} value={params[field.key] ?? ""} onChange={(event) => { setParams({ ...params, [field.key]: event.target.value }); setSaved(false); }} {...control} />}
      </Field>)}
    </fieldset>)}
    {error && <Alert variant="destructive" role="alert">{error}</Alert>}
    {saved && <Alert variant="success" role="status">Настройки алгоритма сохранены.</Alert>}
    <div><Button disabled={busy} type="submit">{busy ? "Сохранение…" : "Сохранить алгоритм"}</Button></div>
    </section></TabsContent>
    <TabsContent value="flexible" forceMount hidden={liveMode !== "flexible"} className="mt-0 grid min-w-0 gap-4">
    <section aria-labelledby={`${optimizerTriggerId}-flexible`} className="grid min-w-0 gap-3">
      <div className="flex items-center justify-center gap-2 text-center"><h4 id={`${optimizerTriggerId}-flexible`} className="min-w-0 font-semibold">Live-сканирование с гибкими параметрами</h4>
        <InfoNote label="О самостоятельных адаптивных профилях">Профиль появляется только после полного принятого исследования. Stop/Play каждой строки не управляет исследованием. Research Stop и изменение очереди сохраняют профили, их рекомендации и историю. Кандидат инициализируется независимо, без повторной проверки исторического результата. Дата профиля — изменение состояния, не дата исследования.</InfoNote></div>
      <AdaptiveLive controller={profiles} />
      <Separator />
      <AdaptiveProfiles algorithm={optimizerAlgorithm} controller={profiles} research={continuous} />
    </section>
    <Separator />
    <ContinuousResearch controller={continuous}>
      <ContinuousControls algorithm={optimizerAlgorithm} controller={continuous}>
        {selected && <ContinuousHistoryCleanup disabled={optimization.creating || optimization.cancelling !== null || optimization.deleting !== null} onRefreshStates={refreshStates} onDeleted={onCleanup} />}
      </ContinuousControls>
    </ContinuousResearch>
    <Separator />
    <section aria-labelledby={`${optimizerTriggerId}-optimizations`} className="grid min-w-0 gap-4">
    <h4 id={`${optimizerTriggerId}-optimizations`} className="text-center font-semibold">Список проведённых исследований</h4>
    {optimization.error && <Alert variant="destructive" role="alert">{optimization.error}<Button type="button" variant="outline" onClick={optimization.refresh}>Обновить статус оптимизации</Button></Alert>}
    <OptimizationHistory id={historyRegionId} algorithmId={algorithm.id} algorithmName={displayName} controller={optimization} active={selected && liveMode === "flexible" && selectedHistory?.open !== true && selectedDeleteTarget === null} onSelect={selectHistory} onDelete={requestDelete} actions={<div className="flex flex-wrap gap-2">
      <Button id={optimizerTriggerId} type="button" variant="outline" aria-label={`Запустить новое исследование ${displayName}`} onClick={() => { setHistorySelection((previous) => previous ? { ...previous, open: false } : null); setOptimizationIntent((value) => value + 1); setOptimizationOpen(true); }}>Запустить новое исследование</Button>
    </div>} />
    </section>
    </TabsContent>
    </Tabs>
  </form>
    <AlgorithmOptimizer key={optimizationIntent} createNew algorithm={optimizerAlgorithm} controller={optimization} open={optimizationOpen} onOpenChange={setOptimizationOpen} returnFocusId={optimizerTriggerId} onDelete={requestDelete} />
    <OptimizationStudyDialog id={selectedHistory?.id} algorithm={algorithm} controller={optimization} open={selectedHistory?.open === true} onOpenChange={(open) => setHistorySelection((previous) => previous ? { ...previous, open } : null)} returnFocusId={selectedHistory?.triggerId ?? historyRegionId} fallbackFocusId={historyRegionId} onDelete={requestDelete} />
    <OptimizationDeleteDialog target={selectedDeleteTarget} controller={optimization} fallbackFocusId={historyRegionId} onClose={() => setDeleteTarget(null)} onDeleted={(id) => {
      if (historySelection?.id === id) setHistorySelection(null);
      if (optimization.byAlgorithm[algorithm.id]?.id === id) setOptimizationOpen(false);
      setDeleteTarget(null);
    }} />
  </>;
}

function hasEnabledLiveMode(algorithm: Pick<Algorithm, "enabled"> & { adaptiveEnabled?: boolean }): boolean {
  return algorithm.enabled || algorithm.adaptiveEnabled === true;
}

function AlgorithmForms({ algorithms }: { algorithms: Algorithm[] }) {
  // One boolean per registered algorithm; hidden panels remain mounted and report their own work.
  const [continuousActivity, setContinuousActivity] = useState<Record<string, boolean>>({});
  const onActivity = useCallback((id: string, active: boolean) => setContinuousActivity((previous) => (previous[id] ?? false) === active ? previous : { ...previous, [id]: active }), []);
  const optimization = useOptimizationStudies(algorithms.map((algorithm) => algorithm.id), algorithms.some((algorithm) => continuousActivity[algorithm.id]));
  const [continuousRevision, setContinuousRevision] = useState(0);
  const [cleanupRevision, setCleanupRevision] = useState(0);
  const [selected, setSelected] = useState(algorithms[0]?.id ?? "");
  const [saved, setSaved] = useState<Record<string, SavedAlgorithm>>({});
  const onSaved = useCallback((id: string, patch: SavedAlgorithm) => setSaved((previous) => ({ ...previous, [id]: { ...previous[id], ...patch } })), []);
  // Keep metadata identity stable during refresh: open forms must not restore their draft over local edits.
  const displayAlgorithms = useMemo(() => algorithms.map((algorithm) => ({ ...algorithm, enabled: saved[algorithm.id]?.enabled ?? algorithm.enabled, externalName: saved[algorithm.id]?.externalName ?? algorithmDisplayName(algorithm) })), [algorithms, saved]);
  const refreshStates = () => { optimization.refresh(); setContinuousRevision((value) => value + 1); };
  const items = algorithms.map((algorithm) => ({ id: algorithm.id, label: saved[algorithm.id]?.externalName ?? algorithmDisplayName(algorithm),
    ...(hasEnabledLiveMode({ enabled: saved[algorithm.id]?.enabled ?? algorithm.enabled, adaptiveEnabled: saved[algorithm.id]?.adaptiveEnabled ?? false }) ? { status: "Live-сканирование включено" } : {}),
  }));
  return <>
    <PageSidebar label="Алгоритмы" items={items} value={selected} onValueChange={setSelected}>
    {displayAlgorithms.map((algorithm) => <PageSidebarPanel key={algorithm.id} value={algorithm.id}>
      <AlgorithmForm algorithm={algorithm} optimization={optimization} onSaved={onSaved} onActivity={onActivity} continuousRevision={continuousRevision} cleanupRevision={cleanupRevision} selected={selected === algorithm.id} refreshStates={refreshStates} onCleanup={() => { refreshStates(); setCleanupRevision((value) => value + 1); }} />
    </PageSidebarPanel>)}
    {algorithms.length === 0 && <p className="text-sm text-muted-foreground">Алгоритмов пока нет.</p>}
  </PageSidebar></>;
}

export function AlgorithmSettings() {
  const [state, setState] = useState<PlatformState<{ algorithms: Algorithm[] }>>({ kind: "loading" });
  useEffect(() => {
    let cancelled = false;
    void fetchPlatform<{ algorithms: Algorithm[] }>("/v1/algorithms").then((result) => { if (!cancelled) setState(result); });
    return () => { cancelled = true; };
  }, []);
  return <div className="grid gap-4">
    {state.kind === "loading" && <p className="text-sm text-muted-foreground">Загрузка алгоритмов…</p>}
    {state.kind === "unreachable" && <Alert variant="destructive" role="alert">Алгоритмы недоступны ({state.detail})</Alert>}
    {state.kind === "ready" && <AlgorithmForms algorithms={state.data.algorithms} />}
  </div>;
}
