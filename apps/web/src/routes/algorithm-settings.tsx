import { useEffect, useId, useState, type FormEvent } from "react";
import { Alert } from "../components/ui/alert";
import { Button } from "../components/ui/button";
import { Card } from "../components/ui/card";
import { Checkbox } from "../components/ui/checkbox";
import { Field } from "../components/ui/field";
import { Input } from "../components/ui/input";
import { fetchPlatform, type PlatformState } from "../lib/platform-api";
import type { OptimizationStudyAction } from "../lib/optimization-api";
import { errorText, writePlatform, type Algorithm } from "./platform-controls";
import { AlgorithmOptimizer, OptimizationStudyDialog } from "./algorithm-optimizer";
import { OptimizationDeleteDialog, OptimizationHistory } from "./optimization-history";
import { useOptimizationStudies, type OptimizationController } from "./use-optimization-studies";

const groups = [
  { title: "Цена и объём", keys: ["priceGrowthPct", "priceDropPct", "windowMinutes", "volumeMultiplier"] },
  { title: "Базовый объём", keys: ["baselineLongDays", "baselineShortDays"] },
  { title: "Качество данных и повторные сигналы", keys: ["cooldownMinutes", "maxGapPercent", "maxConsecutiveGapMinutes", "requiredRecentMinutes"] },
];

function AlgorithmForm({ algorithm, optimization }: { algorithm: Algorithm; optimization: OptimizationController }) {
  const [params, setParams] = useState(algorithm.params);
  const [baseline, setBaseline] = useState(algorithm.params);
  const [optimizationOpen, setOptimizationOpen] = useState(false);
  const [historySelection, setHistorySelection] = useState<{ id: number; open: boolean } | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<OptimizationStudyAction | null>(null);
  const optimizerTriggerId = useId();
  const [enabled, setEnabled] = useState(algorithm.enabled);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const ungrouped = algorithm.fields.filter((field) => !groups.some((group) => group.keys.includes(field.key)));
  const fieldGroups = [...groups, ...(ungrouped.length ? [{ title: "Другие параметры", keys: ungrouped.map((field) => field.key) }] : [])];
  const requestDelete = (study: OptimizationStudyAction) => {
    optimization.clearError(); setDeleteTarget({ id: study.id, status: study.status });
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
      setSaved(true);
    } catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }
  return <><Card asChild><form onSubmit={(event) => { void save(event); }} className="grid gap-4">
    <h3 className="text-lg font-semibold">{algorithm.name} · {algorithm.version}</h3>
    <p className="text-sm text-muted-foreground">{algorithm.description}</p>
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
    {optimization.error && <Alert variant="destructive" role="alert">{optimization.error}<Button type="button" variant="outline" onClick={optimization.refresh}>Обновить статус оптимизации</Button></Alert>}
    <div className="flex flex-wrap gap-2"><Button disabled={busy} type="submit">{busy ? "Сохранение…" : "Сохранить алгоритм"}</Button><Button id={optimizerTriggerId} type="button" variant="outline" aria-label={`Оптимизировать ${algorithm.name}`} onClick={() => { setHistorySelection((previous) => previous ? { ...previous, open: false } : null); setOptimizationOpen(true); }}>{optimization.byAlgorithm[algorithm.id] ? "Статус / результат оптимизации" : "Оптимизировать"}</Button></div>
    <OptimizationHistory algorithmId={algorithm.id} algorithmName={algorithm.name} controller={optimization} onSelect={(id) => { setOptimizationOpen(false); setHistorySelection({ id, open: true }); }} onDelete={requestDelete} />
  </form></Card>
    <AlgorithmOptimizer algorithm={{ ...algorithm, params: baseline }} controller={optimization} open={optimizationOpen} onOpenChange={setOptimizationOpen} returnFocusId={optimizerTriggerId} onDelete={requestDelete} />
    <OptimizationStudyDialog id={historySelection?.id} algorithm={algorithm} controller={optimization} open={historySelection?.open === true} onOpenChange={(open) => setHistorySelection((previous) => previous ? { ...previous, open } : null)} returnFocusId={optimizerTriggerId} onDelete={requestDelete} />
    <OptimizationDeleteDialog target={deleteTarget} controller={optimization} onClose={() => setDeleteTarget(null)} onDeleted={(id) => {
      if (historySelection?.id === id) setHistorySelection(null);
      if (optimization.byAlgorithm[algorithm.id]?.id === id) setOptimizationOpen(false);
      setDeleteTarget(null);
    }} />
  </>;
}

function AlgorithmForms({ algorithms }: { algorithms: Algorithm[] }) {
  const optimization = useOptimizationStudies(algorithms.map((algorithm) => algorithm.id));
  return <>{algorithms.map((algorithm) => <AlgorithmForm key={algorithm.id} algorithm={algorithm} optimization={optimization} />)}</>;
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
