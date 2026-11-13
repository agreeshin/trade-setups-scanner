import { useCallback, useEffect, useId, useState, type FormEvent } from "react";
import { Alert } from "../components/ui/alert";
import { Button } from "../components/ui/button";
import { Checkbox } from "../components/ui/checkbox";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "../components/ui/dialog";
import { Field } from "../components/ui/field";
import { Input } from "../components/ui/input";
import { PageSidebar, PageSidebarPanel } from "../components/ui/page-sidebar";
import { Separator } from "../components/ui/separator";
import { fetchPlatform, type PlatformState } from "../lib/platform-api";
import type { OptimizationStudyAction } from "../lib/optimization-api";
import { algorithmDisplayName, errorText, writePlatform, type Algorithm } from "./platform-controls";
import { AlgorithmOptimizer, OptimizationStudyDialog } from "./algorithm-optimizer";
import { OptimizationDeleteDialog, OptimizationHistory } from "./optimization-history";
import { useOptimizationStudies, type OptimizationController } from "./use-optimization-studies";
import { useContinuousStudy } from "./use-continuous-study";
import { AdaptiveLive, ContinuousResearch } from "./continuous-study";

const groups = [
  { title: "Цена и объём", keys: ["priceGrowthPct", "priceDropPct", "windowMinutes", "volumeMultiplier"] },
  { title: "Базовый объём", keys: ["baselineLongDays", "baselineShortDays"] },
  { title: "Качество данных и повторные сигналы", keys: ["cooldownMinutes", "maxGapPercent", "maxConsecutiveGapMinutes", "requiredRecentMinutes"] },
];

type SavedAlgorithm = Partial<Pick<Algorithm, "enabled" | "externalName">> & { adaptiveEnabled?: boolean };
function AlgorithmForm({ algorithm, optimization, onSaved }: { algorithm: Algorithm; optimization: OptimizationController; onSaved: (id: string, patch: SavedAlgorithm) => void }) {
  const continuous = useContinuousStudy(algorithm.id);
  const displayName = algorithmDisplayName(algorithm);
  const [externalName, setExternalName] = useState(displayName);
  const [nameBusy, setNameBusy] = useState(false);
  const [nameError, setNameError] = useState<string | null>(null);
  const [nameSaved, setNameSaved] = useState(false);
  const { refresh: refreshOptimization } = optimization;
  const adaptiveSaved = continuous.available ? continuous.process?.config?.adaptiveEnabled ?? false : undefined;
  useEffect(() => { if (adaptiveSaved !== undefined) onSaved(algorithm.id, { adaptiveEnabled: adaptiveSaved }); }, [adaptiveSaved, algorithm.id, onSaved]);
  useEffect(() => { if (continuous.revision > 0) refreshOptimization(); }, [continuous.revision, refreshOptimization]);
  const [params, setParams] = useState(algorithm.params);
  const [baseline, setBaseline] = useState(algorithm.params);
  const [optimizationOpen, setOptimizationOpen] = useState(false);
  const [historySelection, setHistorySelection] = useState<{ id: number; open: boolean; triggerId: string } | null>(null);
  const [historyOpen, setHistoryOpen] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<OptimizationStudyAction | null>(null);
  const optimizerTriggerId = useId();
  const historyTriggerId = useId();
  const historyDialogId = useId();
  const [enabled, setEnabled] = useState(algorithm.enabled);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const ungrouped = algorithm.fields.filter((field) => !groups.some((group) => group.keys.includes(field.key)));
  const fieldGroups = [...groups, ...(ungrouped.length ? [{ title: "Другие параметры", keys: ungrouped.map((field) => field.key) }] : [])];
  const requestDelete = (study: OptimizationStudyAction) => {
    optimization.clearError(); setDeleteTarget({ id: study.id, status: study.status });
  };
  const selectHistory = (id: number, triggerId: string) => {
    setOptimizationOpen(false); setHistorySelection({ id, open: true, triggerId });
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
    const name = externalName.trim();
    setNameError(null); setNameSaved(false);
    if ([...name].length < 1 || [...name].length > 120 || /\p{Cc}/u.test(name)) { setNameError("Название: от 1 до 120 символов, без управляющих символов."); return; }
    setNameBusy(true);
    try {
      // A rename is its own partial update. Never resend stale params/enabled with it.
      await writePlatform(`/v1/algorithms/${encodeURIComponent(algorithm.id)}`, { externalName: name });
      setExternalName(name); onSaved(algorithm.id, { externalName: name }); setNameSaved(true);
    } catch (cause) { setNameError(errorText(cause)); }
    finally { setNameBusy(false); }
  }
  return <><form onSubmit={(event) => { void save(event); }} className="grid min-w-0 gap-4">
    <h3 className="text-lg font-semibold">{displayName} · {algorithm.version}</h3>
    <p className="text-sm text-muted-foreground">{algorithm.description}</p>
    <Field label="Внешнее название алгоритма" hint="До 120 символов. Отдельная пользовательская подпись; служебное имя, ID, версия и старые сетапы не меняются.">{(control) => <Input {...control} maxLength={240} value={externalName} onChange={(event) => { setExternalName(event.target.value); setNameSaved(false); }} onKeyDown={(event) => { if (event.key === "Enter") { event.preventDefault(); event.stopPropagation(); if (!nameBusy) void saveName(); } }} />}</Field>
    <div><Button type="button" variant="outline" disabled={nameBusy} onClick={() => { void saveName(); }}>{nameBusy ? "Сохранение названия…" : "Сохранить название"}</Button></div>
    {nameError && <Alert variant="destructive" role="alert">{nameError}</Alert>}
    {nameSaved && <p role="status" className="text-sm">Название алгоритма сохранено.</p>}
    <section aria-labelledby={`${optimizerTriggerId}-constant`} className="grid min-w-0 gap-4">
    <h4 id={`${optimizerTriggerId}-constant`} className="font-semibold">Live-сканирование с постоянными параметрами</h4>
    <label className="flex items-center gap-2 text-sm"><Checkbox checked={enabled} onCheckedChange={(checked) => { setEnabled(checked === true); setSaved(false); }} />Включён для live-сканирования</label>
    {fieldGroups.map((group) => <fieldset key={group.title} className="grid gap-3 sm:grid-cols-2">
      <legend className="mb-2 font-medium">{group.title}</legend>
      {algorithm.fields.filter((field) => group.keys.includes(field.key)).map((field) => <Field key={field.key} label={`${field.label} (${field.unit})`}>
        {(control) => <Input type="number" required min={field.min} max={field.max} step={field.type === "integer" ? "1" : "any"} value={params[field.key] ?? ""} onChange={(event) => { setParams({ ...params, [field.key]: event.target.value }); setSaved(false); }} {...control} />}
      </Field>)}
    </fieldset>)}
    <p className="text-xs text-muted-foreground">Порог цены задаётся в процентах — рост или падение, зависит от алгоритма; окно в минутах. Множитель объёма 3 означает 3× (300% базового объёма). Изменения общие для live и новых исторических запусков; завершённые запуски сохраняют свой снимок параметров.</p>
    {error && <Alert variant="destructive" role="alert">{error}</Alert>}
    {saved && <Alert variant="success" role="status">Настройки алгоритма сохранены.</Alert>}
    <div><Button disabled={busy} type="submit">{busy ? "Сохранение…" : "Сохранить алгоритм"}</Button></div>
    </section>
    <Separator />
    <section aria-labelledby={`${optimizerTriggerId}-flexible`} className="grid min-w-0 gap-3">
      <h4 id={`${optimizerTriggerId}-flexible`} className="font-semibold">Live-сканирование с гибкими параметрами</h4>
      <AdaptiveLive controller={continuous} />
    </section>
    <Separator />
    <section aria-labelledby={`${optimizerTriggerId}-optimizations`} className="grid min-w-0 gap-4">
    <h4 id={`${optimizerTriggerId}-optimizations`} className="font-semibold">Оптимизации</h4>
    {optimization.error && <Alert variant="destructive" role="alert">{optimization.error}<Button type="button" variant="outline" onClick={optimization.refresh}>Обновить статус оптимизации</Button></Alert>}
    <div className="flex flex-wrap gap-2">
      <Button id={optimizerTriggerId} type="button" variant="outline" aria-label={`Оптимизировать ${displayName}`} onClick={() => { setHistoryOpen(false); setHistorySelection((previous) => previous ? { ...previous, open: false } : null); setOptimizationOpen(true); }}>Оптимизировать</Button>
      <Button id={historyTriggerId} type="button" variant="outline" onClick={() => { setOptimizationOpen(false); setHistorySelection((previous) => previous ? { ...previous, open: false } : null); setHistoryOpen(true); }}>Посмотреть весь список</Button>
    </div>
    <ContinuousResearch algorithm={{ ...algorithm, params: baseline }} controller={continuous} />
    <OptimizationHistory algorithmId={algorithm.id} algorithmName={displayName} controller={optimization} pageSize={5} pagination={false} onSelect={selectHistory} onDelete={requestDelete} />
    </section>
  </form>
    <Dialog open={historyOpen} onOpenChange={setHistoryOpen}>
      <DialogContent id={historyDialogId} tabIndex={-1} className="max-w-5xl" onCloseAutoFocus={(event) => { event.preventDefault(); document.getElementById(historyTriggerId)?.focus(); }}>
        <DialogHeader><DialogTitle>Все оптимизации · {displayName}</DialogTitle><DialogDescription>Полная история исследований алгоритма. ID открывает сохранённый результат, не закрывая список.</DialogDescription></DialogHeader>
        <div className="grid min-h-0 min-w-0 gap-4 overflow-y-auto pr-1">
          {optimization.error && <Alert variant="destructive" role="alert">{optimization.error}<Button type="button" variant="outline" onClick={optimization.refresh}>Обновить статус оптимизации</Button></Alert>}
          {historyOpen && <OptimizationHistory algorithmId={algorithm.id} algorithmName={displayName} controller={optimization} pageSize={10} active={historySelection?.open !== true && deleteTarget === null} onSelect={selectHistory} onDelete={requestDelete} />}
        </div>
      </DialogContent>
    </Dialog>
    <AlgorithmOptimizer algorithm={{ ...algorithm, params: baseline }} controller={optimization} open={optimizationOpen} onOpenChange={setOptimizationOpen} returnFocusId={optimizerTriggerId} onDelete={requestDelete} />
    <OptimizationStudyDialog id={historySelection?.id} algorithm={algorithm} controller={optimization} open={historySelection?.open === true} onOpenChange={(open) => setHistorySelection((previous) => previous ? { ...previous, open } : null)} returnFocusId={historySelection?.triggerId ?? optimizerTriggerId} fallbackFocusId={historyOpen ? historyDialogId : optimizerTriggerId} onDelete={requestDelete} />
    <OptimizationDeleteDialog target={deleteTarget} controller={optimization} onClose={() => setDeleteTarget(null)} onDeleted={(id) => {
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
  const optimization = useOptimizationStudies(algorithms.map((algorithm) => algorithm.id));
  const [selected, setSelected] = useState(algorithms[0]?.id ?? "");
  const [saved, setSaved] = useState<Record<string, SavedAlgorithm>>({});
  const onSaved = useCallback((id: string, patch: SavedAlgorithm) => setSaved((previous) => ({ ...previous, [id]: { ...previous[id], ...patch } })), []);
  const items = algorithms.map((algorithm) => ({ id: algorithm.id, label: saved[algorithm.id]?.externalName ?? algorithmDisplayName(algorithm),
    ...(hasEnabledLiveMode({ enabled: saved[algorithm.id]?.enabled ?? algorithm.enabled, adaptiveEnabled: saved[algorithm.id]?.adaptiveEnabled ?? false }) ? { status: "Live-сканирование включено" } : {}),
  }));
  return <PageSidebar label="Алгоритмы" items={items} value={selected} onValueChange={setSelected}>
    {algorithms.map((algorithm) => <PageSidebarPanel key={algorithm.id} value={algorithm.id}>
      <AlgorithmForm algorithm={{ ...algorithm, externalName: saved[algorithm.id]?.externalName ?? algorithmDisplayName(algorithm) }} optimization={optimization} onSaved={onSaved} />
    </PageSidebarPanel>)}
    {algorithms.length === 0 && <p className="text-sm text-muted-foreground">Алгоритмов пока нет.</p>}
  </PageSidebar>;
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
