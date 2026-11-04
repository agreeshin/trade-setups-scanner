import { useEffect, useState, type FormEvent } from "react";
import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import { fetchPlatform, type PlatformState } from "../lib/platform-api";
import { errorText, writePlatform, type Algorithm } from "./platform-controls";

const groups = [
  { title: "Цена и объём", keys: ["priceGrowthPct", "windowMinutes", "volumeMultiplier"] },
  { title: "Базовый объём", keys: ["baselineLongDays", "baselineShortDays"] },
  { title: "Качество данных и повторные сигналы", keys: ["cooldownMinutes", "maxGapPercent", "maxConsecutiveGapMinutes", "requiredRecentMinutes"] },
];

function AlgorithmForm({ algorithm }: { algorithm: Algorithm }) {
  const [params, setParams] = useState(algorithm.params);
  const [enabled, setEnabled] = useState(algorithm.enabled);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
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
      setSaved(true);
    } catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }
  return <form onSubmit={(event) => { void save(event); }} className="mt-4 grid gap-4 rounded-md border border-input p-4">
    <h3 className="text-lg font-semibold">{algorithm.name} · {algorithm.version}</h3>
    <p className="text-sm text-muted-foreground">{algorithm.description}</p>
    <label className="flex items-center gap-2 text-sm"><Input type="checkbox" className="h-4 w-4" checked={enabled} onChange={(event) => { setEnabled(event.target.checked); setSaved(false); }} />Включён для live-сканирования</label>
    {groups.map((group) => <fieldset key={group.title} className="grid gap-3 sm:grid-cols-2">
      <legend className="mb-2 font-medium">{group.title}</legend>
      {algorithm.fields.filter((field) => group.keys.includes(field.key)).map((field) => <label key={field.key} className="grid gap-1 text-sm">
        {field.label} ({field.unit})
        <Input type="number" required min={field.min} max={field.max} step={field.type === "integer" ? "1" : "any"} value={params[field.key] ?? ""} onChange={(event) => { setParams({ ...params, [field.key]: event.target.value }); setSaved(false); }} />
      </label>)}
    </fieldset>)}
    <p className="text-xs text-muted-foreground">Рост цены задаётся в процентах, окно в минутах. Множитель объёма 3 означает 3× (300% базового объёма). Изменения общие для live и новых исторических запусков; завершённые запуски сохраняют свой снимок параметров.</p>
    {error && <p role="alert">{error}</p>}
    {saved && <p role="status">Настройки алгоритма сохранены.</p>}
    <Button disabled={busy} type="submit">{busy ? "Сохранение…" : "Сохранить алгоритм"}</Button>
  </form>;
}

export function AlgorithmSettings() {
  const [state, setState] = useState<PlatformState<{ algorithms: Algorithm[] }>>({ kind: "loading" });
  useEffect(() => {
    let cancelled = false;
    void fetchPlatform<{ algorithms: Algorithm[] }>("/v1/algorithms").then((result) => { if (!cancelled) setState(result); });
    return () => { cancelled = true; };
  }, []);
  return <section aria-labelledby="algorithms-heading" className="mt-8 max-w-3xl">
    <h2 id="algorithms-heading" className="text-xl font-semibold">Алгоритмы: live и история</h2>
    {state.kind === "loading" && <p>Загрузка алгоритмов…</p>}
    {state.kind === "unreachable" && <p role="alert">Алгоритмы недоступны ({state.detail})</p>}
    {state.kind === "ready" && state.data.algorithms.map((algorithm) => <AlgorithmForm key={algorithm.id} algorithm={algorithm} />)}
  </section>;
}
