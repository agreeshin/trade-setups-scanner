import { useEffect, useRef, useState, type FormEvent } from "react";
import { Link } from "react-router";
import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import { Select } from "../components/ui/select";
import { apiFetch, fetchPlatform, type PlatformState } from "../lib/platform-api";
import { errorText, localDateInput, localTime, localTimezone, replayRange, savePreference, writePlatform, type Algorithm, type SetupRow } from "./platform-controls";
import { SetupTable } from "./setup-table";
import { InstrumentPicker, type CatalogInstrument } from "./instrument-picker";
import { usePlatformPoll } from "./use-platform-poll";

interface RunRow {
  id: number;
  symbol: string;
  status: string;
  cacheHit: boolean;
  setupCount: number;
  error: string | null;
  algorithmId: string;
  algorithmVersion: string;
  fromUnix: number;
  toUnix: number;
  createdAt: string;
  params: Record<string, string | number>;
}
interface ReplayForm { symbols: string; from: string; to: string; algorithmId: string }
interface RunDetail { run: RunRow; setups: SetupRow[]; offset: number; limit: number; total: number }
const parameterLabels: Record<string, string> = {
  priceGrowthPct: "Рост цены (%)", windowMinutes: "Окно (мин)", volumeMultiplier: "Множитель объёма (×)",
  baselineLongDays: "Длинная база (дни)", baselineShortDays: "Короткая база (дни)", cooldownMinutes: "Пауза между сигналами (мин)",
  maxGapPercent: "Допустимые пропуски (%)", maxConsecutiveGapMinutes: "Пропуски подряд (мин)", requiredRecentMinutes: "Непрерывная недавняя история (мин)",
};

function RunResultPage({ id, offset, revision, onPage }: { id: number; offset: number; revision: number; onPage: (offset: number) => void }) {
  const state = usePlatformPoll<RunDetail>(`/v1/replay-runs/${id}?offset=${offset}&limit=100`, revision);
  return <>
    {state.kind === "loading" && <p role="status">Загрузка результатов…</p>}
    {state.kind === "unreachable" && <p role="alert">Не удалось загрузить результаты ({state.detail})</p>}
    {state.kind === "ready" && <>
      <p className="mt-2 text-sm">{state.data.run.symbol} · {state.data.run.algorithmId} · {state.data.run.algorithmVersion} · {state.data.run.status}</p>
      <p className="text-sm text-muted-foreground">{localTime(state.data.run.fromUnix)} – {localTime(state.data.run.toUnix)} ({localTimezone})</p>
      <details className="mt-3"><summary className="cursor-pointer font-medium">Снимок параметров запуска</summary><dl className="mt-2 grid gap-2 text-sm sm:grid-cols-2">{Object.entries(state.data.run.params).map(([key, value]) => <div key={key} className="break-words"><dt className="text-muted-foreground">{parameterLabels[key] ?? key}</dt><dd>{value}</dd></div>)}</dl></details>
      {state.data.run.error && <p role="alert">{state.data.run.error}</p>}
      <SetupTable setups={state.data.setups.slice(0, 100)} />
      <nav aria-label="Страницы результатов запуска" className="mt-4 flex flex-wrap items-center gap-3">
        <Button variant="outline" disabled={offset === 0} onClick={() => onPage(Math.max(0, offset - 100))}>Предыдущая страница</Button>
        <span aria-live="polite">Всего сетапов: {state.data.total}. Страница {Math.floor(offset / 100) + 1} из {Math.max(1, Math.ceil(state.data.total / 100))}</span>
        <Button variant="outline" disabled={offset + 100 >= state.data.total} onClick={() => onPage(offset + 100)}>Следующая страница</Button>
      </nav>
    </>}
  </>;
}

function RunResults({ id, revision }: { id: number; revision: number }) {
  const [offset, setOffset] = useState(0);
  const [downloading, setDownloading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const download = useRef<{ controller?: AbortController; url?: string | undefined; timer?: ReturnType<typeof setTimeout> }>({});
  useEffect(() => {
    const resource = download.current;
    return () => { resource.controller?.abort(); clearTimeout(resource.timer); if (resource.url) URL.revokeObjectURL(resource.url); };
  }, []);
  async function exportCsv() {
    const resource = download.current;
    clearTimeout(resource.timer);
    if (resource.url) { URL.revokeObjectURL(resource.url); resource.url = undefined; }
    const controller = new AbortController();
    resource.controller = controller;
    setDownloading(true); setError(null);
    try {
      const response = await apiFetch(`/v1/replay-runs/${id}/export.csv`, { signal: controller.signal });
      if (!response.ok) throw new Error(`Экспорт недоступен: HTTP ${response.status}`);
      const blob = await response.blob();
      if (controller.signal.aborted) return;
      resource.url = URL.createObjectURL(blob);
      const link = document.createElement("a");
      try {
        link.href = resource.url; link.download = `replay-run-${id}.csv`; document.body.append(link); link.click();
      } finally {
        link.remove();
        resource.timer = setTimeout(() => { if (resource.url) URL.revokeObjectURL(resource.url); resource.url = undefined; }, 1000);
      }
    } catch (cause) { if (!controller.signal.aborted) setError(errorText(cause)); }
    finally { if (!controller.signal.aborted) setDownloading(false); }
  }
  return <section className="mt-6 rounded-md border border-input p-4" aria-labelledby="run-results-heading">
    <h2 id="run-results-heading" className="text-lg font-semibold">Результаты запуска #{id}</h2>
    <Button className="mt-3" variant="outline" disabled={downloading} onClick={() => { void exportCsv(); }}>{downloading ? "Подготовка CSV…" : "Скачать CSV запуска"}</Button>
    <p className="mt-2 text-xs text-muted-foreground">CSV содержит все результаты запуска, не только текущую страницу. Большой файл потребует времени и памяти браузера.</p>
    {error && <p role="alert">{error}</p>}
    <RunResultPage key={offset} id={id} offset={offset} revision={revision} onPage={setOffset} />
  </section>;
}

export function RunsPage() {
  const [form, setForm] = useState<ReplayForm>({ symbols: "", from: "", to: "", algorithmId: "" });
  const [algorithms, setAlgorithms] = useState<PlatformState<{ algorithms: Algorithm[] }>>({ kind: "loading" });
  const [catalog, setCatalog] = useState<PlatformState<{ instruments: CatalogInstrument[] }>>({ kind: "loading" });
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const [selected, setSelected] = useState<number | null>(null);
  const [reload, setReload] = useState(0);
  const runs = usePlatformPoll<{ runs: RunRow[] }>("/v1/replay-runs", reload);
  const [cancelling, setCancelling] = useState<number | null>(null);
  useEffect(() => {
    let cancelled = false;
    void Promise.all([
      fetchPlatform<{ algorithms: Algorithm[] }>("/v1/algorithms"),
      fetchPlatform<{ value: { replay?: Partial<ReplayForm> & { fromUnix?: number; toUnix?: number } } }>("/v1/settings/ui"),
      fetchPlatform<{ instruments: CatalogInstrument[] }>("/v1/instruments"),
    ]).then(([algorithmsResult, preferences, instrumentsResult]) => {
      if (cancelled) return;
      setAlgorithms(algorithmsResult);
      const instruments = instrumentsResult.kind === "ready" ? instrumentsResult.data.instruments.filter((instrument) => instrument.exchange === "bybit" && instrument.category === "linear") : [];
      setCatalog(instrumentsResult.kind === "ready" ? { kind: "ready", data: { instruments } } : instrumentsResult);
      const saved = preferences.kind === "ready" ? preferences.data.value.replay : undefined;
      setForm({ symbols: typeof saved?.symbols === "string" ? saved.symbols : instruments.map((instrument) => instrument.symbol).join("\n"), from: typeof saved?.fromUnix === "number" ? localDateInput(saved.fromUnix) : typeof saved?.from === "string" ? saved.from : "", to: typeof saved?.toUnix === "number" ? localDateInput(saved.toUnix) : typeof saved?.to === "string" ? saved.to : "", algorithmId: typeof saved?.algorithmId === "string" ? saved.algorithmId : algorithmsResult.kind === "ready" ? algorithmsResult.data.algorithms[0]?.id ?? "" : "" });
      if (preferences.kind === "unreachable") setError(`Не удалось загрузить сохранённую форму (${preferences.detail})`);
      setReady(true);
    });
    return () => { cancelled = true; };
  }, []);
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setError(null);
    setMessage(null);
    try {
      const range = replayRange(form.from, form.to);
      const symbols = [...new Set(form.symbols.split(/[\s,;]+/).map((item) => item.trim().toUpperCase()).filter(Boolean))];
      if (!symbols.length || symbols.length > 100 || symbols.some((symbol) => !/^[A-Z0-9]{2,40}$/.test(symbol))) throw new Error("Укажите от 1 до 100 корректных символов, например BTCUSDT.");
      if (catalog.kind !== "ready" || symbols.some((symbol) => !catalog.data.instruments.some((instrument) => instrument.symbol === symbol))) throw new Error("Все выбранные инструменты должны присутствовать в каталоге БД Bybit Linear.");
      if (algorithms.kind !== "ready" || !algorithms.data.algorithms.some((algorithm) => algorithm.id === form.algorithmId)) throw new Error("Выберите доступный алгоритм.");
      setBusy(true);
      await writePlatform("/v1/replay-runs", { async: true, algorithmId: form.algorithmId, instruments: symbols.map((symbol) => ({ exchange: "bybit", category: "linear", symbol })), ...range }, "POST");
      setMessage("Запрос запуска принят. Результаты и статус обновляются в списке ниже.");
      setReload((value) => value + 1);
    } catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }
  async function saveForm() {
    setBusy(true); setError(null); setMessage(null);
    try { await savePreference("replay", { symbols: form.symbols, algorithmId: form.algorithmId, ...replayRange(form.from, form.to) }); setMessage("Форма сохранена в базе сигналов."); }
    catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }
  async function cancelRun(id: number) {
    setCancelling(id); setError(null); setMessage(null);
    try { await writePlatform(`/v1/replay-runs/${id}/cancel`, {}, "POST"); setMessage(`Отмена запуска #${id} запрошена. Дождитесь конечного статуса.`); setReload((value) => value + 1); }
    catch (cause) { setError(errorText(cause)); }
    finally { setCancelling(null); }
  }
  return <section aria-labelledby="runs-heading">
    <h1 id="runs-heading" className="text-2xl font-semibold">Исторические запуски</h1>
    <form onSubmit={(event) => { void submit(event); }} className="mt-4 grid max-w-2xl gap-4">
      <label className="grid gap-1 text-sm">Алгоритм<Select required disabled={!ready || algorithms.kind !== "ready"} value={form.algorithmId} onChange={(event) => setForm({ ...form, algorithmId: event.target.value })}>
        <option value="">Выберите алгоритм</option>
        {algorithms.kind === "ready" && algorithms.data.algorithms.map((algorithm) => <option key={algorithm.id} value={algorithm.id}>{algorithm.name} · {algorithm.version}</option>)}
      </Select></label>
      {algorithms.kind === "unreachable" && <p role="alert">Алгоритмы недоступны ({algorithms.detail})</p>}
      <p className="text-sm text-muted-foreground">Запуск использует сохранённый снимок общих параметров. <Link className="underline" to="/settings">Настроить алгоритмы</Link></p>
      {catalog.kind === "loading" && <p>Загрузка инструментов из БД…</p>}
      {catalog.kind === "unreachable" && <p role="alert">Каталог БД недоступен ({catalog.detail}). Запуск заблокирован.</p>}
      {catalog.kind === "ready" && <InstrumentPicker catalog={catalog.data.instruments} symbols={form.symbols} onChange={(symbols) => setForm({ ...form, symbols })} disabled={!ready || busy} />}
      <p id="timezone" className="text-sm text-muted-foreground">Локальное время: {localTimezone}. В API передаются Unix-секунды.</p>
      <div className="grid gap-3 sm:grid-cols-2">
        <label className="grid gap-1 text-sm">Начало<Input type="datetime-local" required aria-describedby="timezone" disabled={!ready} value={form.from} onChange={(event) => setForm({ ...form, from: event.target.value })} /></label>
        <label className="grid gap-1 text-sm">Конец<Input type="datetime-local" required aria-describedby="timezone" disabled={!ready} value={form.to} onChange={(event) => setForm({ ...form, to: event.target.value })} /></label>
      </div>
      {error && <p role="alert">{error}</p>}{message && <p role="status">{message}</p>}
      <p className="text-xs text-muted-foreground">Расчёт выполняется в фоне. Список обновляется каждые 3 секунды в видимой вкладке. Закрытие страницы не отменяет расчёт; используйте кнопку отмены.</p>
      <div className="flex flex-wrap gap-2"><Button type="submit" disabled={busy || !ready || algorithms.kind !== "ready" || catalog.kind !== "ready"}>{busy ? "Выполняется…" : "Запустить"}</Button><Button type="button" variant="outline" disabled={busy || !ready} onClick={() => { void saveForm(); }}>Сохранить форму</Button></div>
    </form>
    <div className="mt-8 flex flex-wrap items-center gap-3"><h2 className="text-lg font-semibold">Последние запуски</h2><Button variant="outline" onClick={() => setReload((value) => value + 1)}>Обновить</Button></div>
    {runs.kind === "loading" && <p>Загрузка…</p>}
    {runs.kind === "unreachable" && <p role="alert">Запуски недоступны ({runs.detail})</p>}
    {runs.kind === "ready" && <div className="mt-3 overflow-x-auto"><table className="w-full text-sm">
      <caption className="text-left text-muted-foreground">До 200 последних запусков. Время: {localTimezone}</caption>
      <thead><tr className="border-b border-input text-left"><th className="p-2">ID</th><th className="p-2">Инструмент</th><th className="p-2">Алгоритм</th><th className="p-2">Статус</th><th className="p-2">Кеш</th><th className="p-2">Сетапов</th><th className="p-2">Действие</th></tr></thead>
      <tbody>{runs.data.runs.slice(0, 200).map((run) => <tr key={run.id} className="border-b border-input">
        <td className="p-2"><Button variant="outline" onClick={() => setSelected(run.id)} aria-label={`Результаты запуска ${run.id}`}>#{run.id}</Button></td><td className="p-2">{run.symbol}</td><td className="p-2">{run.algorithmId} · {run.algorithmVersion}</td>
        <td className="p-2">{({ queued: "В очереди", running: "Выполняется", cancel_requested: "Отмена запрошена" } as Record<string, string>)[run.status] ?? run.status}</td><td className="p-2">{run.cacheHit ? "да" : "нет"}</td><td className="p-2">{run.setupCount}</td>
        <td className="p-2">{["queued", "running", "cancel_requested"].includes(run.status) && <Button variant="outline" disabled={cancelling !== null || run.status === "cancel_requested"} onClick={() => { void cancelRun(run.id); }} aria-label={`Отменить запуск ${run.id}`}>Отменить</Button>}</td>
      </tr>)}</tbody></table>{runs.data.runs.length === 0 && <p>Запусков пока нет.</p>}</div>}
    {selected !== null && <RunResults key={selected} id={selected} revision={reload} />}
  </section>;
}
