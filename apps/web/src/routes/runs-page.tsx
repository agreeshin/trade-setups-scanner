import { useEffect, useRef, useState, type FormEvent } from "react";
import { Link } from "react-router";
import { Alert } from "../components/ui/alert";
import { Badge, runStatusVariant } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import { Card } from "../components/ui/card";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "../components/ui/dialog";
import { Field } from "../components/ui/field";
import { Input } from "../components/ui/input";
import { PageHeader, SectionHeader } from "../components/ui/page-header";
import { Pagination } from "../components/ui/pagination";
import { Select } from "../components/ui/select";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "../components/ui/table";
import { apiFetch, fetchPlatform, type PlatformState } from "../lib/platform-api";
import { parameterLabels } from "../lib/parameter-labels";
import { errorText, localDateInput, localTime, localTimezoneLabel, replayRange, savePreference, utcMidnightOnDateChange, writePlatform, type Algorithm, type SetupRow } from "./platform-controls";
import { SetupTable } from "./setup-table";
import { InstrumentPicker, type CatalogInstrument } from "./instrument-picker";
import { usePlatformPoll } from "./use-platform-poll";
import { EmptyReplayResult, ReplayQualitySummary, qualityLabel, type QualityPanelState, type ReplayQuality } from "./replay-quality";
import { ReplayPreflight, type PreflightResponse, type ReplayInstrument } from "./replay-preflight";

interface RunRow {
  id: number;
  name: string;
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
  quality?: ReplayQuality | null;
  qualityComputedAt?: string | null;
  qualitySourceRunId?: number | null;
  isBatch?: boolean;
  instrumentCount?: number;
  activeInstruments?: number;
  doneInstruments?: number;
  failedInstruments?: number;
}
interface ReplayForm { symbols: string; from: string; to: string; algorithmId: string }
interface RunDetail { run: RunRow; setups: SetupRow[]; offset: number; limit: number; total: number }
const RUN_STATUS_LABELS: Record<string, string> = { queued: "В очереди", running: "Выполняется", cancel_requested: "Отмена запрошена" };

function RunResultPage({ id, isBatch, offset, revision, onPage, qualityPanel, onQualityPanel }: { id: number; isBatch: boolean; offset: number; revision: number; onPage: (offset: number) => void; qualityPanel: QualityPanelState; onQualityPanel: (value: QualityPanelState) => void }) {
  const state = usePlatformPoll<RunDetail>(`${isBatch ? `/v1/replay-batches/${id}` : `/v1/replay-runs/${id}`}?offset=${offset}&limit=100`, revision);
  return <>
    {state.kind === "loading" && <p role="status">Загрузка результатов…</p>}
    {state.kind === "unreachable" && <Alert variant="destructive" role="alert">Не удалось загрузить результаты ({state.detail})</Alert>}
    {state.kind === "ready" && <>
      <p className="mt-2 break-words text-sm">{state.data.run.name} · {state.data.run.symbol} · {state.data.run.algorithmId} · {state.data.run.algorithmVersion} · {state.data.run.status}</p>
      <p className="text-sm text-muted-foreground">{localTime(state.data.run.fromUnix)} – {localTime(state.data.run.toUnix)} ({localTimezoneLabel()})</p>
      <details className="mt-3"><summary className="cursor-pointer font-medium">Снимок параметров запуска</summary><dl className="mt-2 grid gap-2 text-sm sm:grid-cols-2">{Object.entries(state.data.run.params).map(([key, value]) => <div key={key} className="break-words"><dt className="text-muted-foreground">{parameterLabels[key] ?? key}</dt><dd>{value}</dd></div>)}</dl></details>
      {state.data.run.error && <Alert variant="destructive" role="alert">{state.data.run.error}</Alert>}
      {!isBatch && <ReplayQualitySummary run={state.data.run} panel={qualityPanel} onPanel={onQualityPanel} revision={revision} />}
      {state.data.total === 0 ? <EmptyReplayResult quality={state.data.run.quality} /> : <SetupTable setups={state.data.setups.slice(0, 100)} />}
      <Pagination
        page={Math.floor(offset / 100) + 1}
        pages={Math.max(1, Math.ceil(state.data.total / 100))}
        onPrev={() => onPage(Math.max(0, offset - 100))}
        onNext={() => onPage(offset + 100)}
        prevDisabled={offset === 0}
        nextDisabled={offset + 100 >= state.data.total}
        summary={`Всего сетапов: ${state.data.total}. Страница ${Math.floor(offset / 100) + 1} из ${Math.max(1, Math.ceil(state.data.total / 100))}`}
        label="Страницы результатов запуска"
        prevLabel="Предыдущая страница"
        nextLabel="Следующая страница"
      />
    </>}
  </>;
}

function RunResults({ id, isBatch, revision }: { id: number; isBatch: boolean; revision: number }) {
  const [offset, setOffset] = useState(0);
  const [qualityPanel, setQualityPanel] = useState<QualityPanelState>({ open: false, offset: 0 });
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
      const response = await apiFetch(isBatch ? `/v1/replay-batches/${id}/export.csv` : `/v1/replay-runs/${id}/export.csv`, { signal: controller.signal });
      if (!response.ok) throw new Error(`Экспорт недоступен: HTTP ${response.status}`);
      const blob = await response.blob();
      if (controller.signal.aborted) return;
      resource.url = URL.createObjectURL(blob);
      const link = document.createElement("a");
      try {
        link.href = resource.url; link.download = isBatch ? `replay-batch-${id}.csv` : `replay-run-${id}.csv`; document.body.append(link); link.click();
      } finally {
        link.remove();
        resource.timer = setTimeout(() => { if (resource.url) URL.revokeObjectURL(resource.url); resource.url = undefined; }, 1000);
      }
    } catch (cause) { if (!controller.signal.aborted) setError(errorText(cause)); }
    finally { if (!controller.signal.aborted) setDownloading(false); }
  }
  return <section className="mt-8" aria-labelledby="run-results-heading">
    <Card className="grid gap-3">
      <SectionHeader id="run-results-heading" title={`Результаты запуска #${id}`} actions={<Button variant="outline" size="sm" disabled={downloading} onClick={() => { void exportCsv(); }}>{downloading ? "Подготовка CSV…" : "Скачать CSV запуска"}</Button>} />
      <p className="text-xs text-muted-foreground">CSV содержит все результаты запуска, не только текущую страницу. Большой файл потребует времени и памяти браузера.</p>
      {error && <Alert variant="destructive" role="alert">{error}</Alert>}
      <RunResultPage key={offset} id={id} isBatch={isBatch} offset={offset} revision={revision} onPage={setOffset} qualityPanel={qualityPanel} onQualityPanel={setQualityPanel} />
    </Card>
  </section>;
}

function TrashIcon() {
  return <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="h-4 w-4" aria-hidden="true"><path d="M3 6h18" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6" /><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" /><line x1="10" x2="10" y1="11" y2="17" /><line x1="14" x2="14" y1="11" y2="17" /></svg>;
}

function PencilIcon() {
  return <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="h-4 w-4" aria-hidden="true"><path d="m16 3 5 5-12 12-6 1 1-6Z" /><path d="m14 5 5 5" /></svg>;
}

function RunNameEditor({ run, onSaved }: { run: RunRow; onSaved: () => void }) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(run.name);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const request = useRef<AbortController | null>(null);
  const errorId = `run-name-error-${run.isBatch === true ? "batch" : "run"}-${run.id}`;
  useEffect(() => () => { request.current?.abort(); }, []);
  async function saveName() {
    if (request.current) return;
    const name = draft.trim();
    if (!name || [...name].length > 120) {
      setError(!name ? "Введите имя запуска." : "Имя запуска должно содержать не более 120 символов.");
      return;
    }
    if (name === run.name) { setEditing(false); setError(null); return; }
    const controller = new AbortController();
    request.current = controller;
    setSaving(true); setError(null);
    try {
      const response = await apiFetch(run.isBatch === true ? `/v1/replay-batches/${run.id}` : `/v1/replay-runs/${run.id}`, {
        method: "PATCH", headers: { "content-type": "application/json" }, body: JSON.stringify({ name }), signal: controller.signal,
      });
      if (!response.ok) throw new Error(`Не удалось сохранить имя запуска: HTTP ${response.status}`);
      if (!controller.signal.aborted) { setEditing(false); onSaved(); }
    } catch (cause) { if (!controller.signal.aborted) setError(errorText(cause)); }
    finally { if (!controller.signal.aborted) { request.current = null; setSaving(false); } }
  }
  return <div className="w-56">
    {editing ? <Input
      autoFocus
      aria-label={`Имя запуска ${run.id}`}
      aria-invalid={error !== null}
      aria-describedby={error ? errorId : undefined}
      title="Enter — сохранить, Escape — отменить"
      value={draft}
      disabled={saving}
      onChange={(event) => { setDraft(event.target.value); setError(null); }}
      onBlur={() => { void saveName(); }}
      onKeyDown={(event) => {
        if (event.nativeEvent.isComposing) return;
        if (event.key === "Enter") { event.preventDefault(); void saveName(); }
        if (event.key === "Escape") { event.preventDefault(); setEditing(false); setError(null); }
      }}
    /> : <div className="flex items-center gap-1">
      <span className="min-w-0 flex-1 truncate" title={run.name}>{run.name}</span>
      <Button type="button" variant="ghost" size="sm" aria-label={`Переименовать запуск ${run.id}`} onClick={() => { setDraft(run.name); setError(null); setEditing(true); }}><PencilIcon /></Button>
    </div>}
    {saving && <p className="mt-1 text-xs text-muted-foreground" role="status">Сохранение…</p>}
    {error && <Alert id={errorId} className="mt-1" variant="destructive" role="alert">{error}</Alert>}
  </div>;
}

export function RunsPage() {
  const [form, setForm] = useState<ReplayForm>({ symbols: "", from: "", to: "", algorithmId: "" });
  const normalizedSymbols = new Set(form.symbols.split(/[\s,;]+/).map((item) => item.trim().toUpperCase()).filter(Boolean));
  const [algorithms, setAlgorithms] = useState<PlatformState<{ algorithms: Algorithm[] }>>({ kind: "loading" });
  const [catalog, setCatalog] = useState<PlatformState<{ instruments: CatalogInstrument[] }>>({ kind: "loading" });
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const [selected, setSelected] = useState<{ id: number; isBatch: boolean } | null>(null);
  const [reload, setReload] = useState(0);
  const runs = usePlatformPoll<{ runs: RunRow[] }>("/v1/replay-runs", reload);
  const [cancelling, setCancelling] = useState<number | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<{ id: number; symbol: string; isBatch: boolean } | null>(null);
  const [deleting, setDeleting] = useState(false);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const [purgeArmed, setPurgeArmed] = useState(false);
  const [purging, setPurging] = useState(false);
  const [knownInstruments, setKnownInstruments] = useState<ReplayInstrument[]>([]);
  const [preview, setPreview] = useState<PreflightResponse | null>(null);
  const [checking, setChecking] = useState(false);
  const checkRequest = useRef<AbortController | null>(null);
  useEffect(() => () => { checkRequest.current?.abort(); }, []);
  function changeForm(next: ReplayForm) {
    checkRequest.current?.abort(); setChecking(false); setPreview(null); setError(null); setMessage(null); setForm(next);
  }
  useEffect(() => {
    let cancelled = false;
    void Promise.all([
      fetchPlatform<{ algorithms: Algorithm[] }>("/v1/algorithms"),
      fetchPlatform<{ value: { replay?: Partial<ReplayForm> & { fromUnix?: number; toUnix?: number; instruments?: ReplayInstrument[] } } }>("/v1/settings/ui"),
      fetchPlatform<{ instruments: CatalogInstrument[] }>("/v1/instruments"),
    ]).then(([algorithmsResult, preferences, instrumentsResult]) => {
      if (cancelled) return;
      setAlgorithms(algorithmsResult);
      const instruments = instrumentsResult.kind === "ready" ? instrumentsResult.data.instruments.filter((instrument) => instrument.exchange === "bybit" && instrument.category === "linear") : [];
      setCatalog(instrumentsResult.kind === "ready" ? { kind: "ready", data: { instruments } } : instrumentsResult);
      const saved = preferences.kind === "ready" ? preferences.data.value.replay : undefined;
      const savedIdentities = Array.isArray(saved?.instruments) ? saved.instruments.filter((item) => item.exchange === "bybit" && item.category === "linear" && typeof item.symbol === "string") : [];
      setKnownInstruments(instrumentsResult.kind === "ready" ? instruments : savedIdentities);
      setForm({ symbols: typeof saved?.symbols === "string" ? saved.symbols : (instrumentsResult.kind === "ready" ? instruments : savedIdentities).map((instrument) => instrument.symbol).join("\n"), from: typeof saved?.fromUnix === "number" ? localDateInput(saved.fromUnix) : typeof saved?.from === "string" ? saved.from : "", to: typeof saved?.toUnix === "number" ? localDateInput(saved.toUnix) : typeof saved?.to === "string" ? saved.to : "", algorithmId: typeof saved?.algorithmId === "string" ? saved.algorithmId : algorithmsResult.kind === "ready" ? algorithmsResult.data.algorithms[0]?.id ?? "" : "" });
      if (preferences.kind === "unreachable") setError(`Не удалось загрузить сохранённую форму (${preferences.detail})`);
      setReady(true);
    });
    return () => { cancelled = true; };
  }, []);
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    checkRequest.current?.abort();
    const controller = new AbortController(); checkRequest.current = controller;
    setPreview(null);
    setError(null);
    setMessage(null);
    try {
      const range = replayRange(form.from, form.to);
      const symbols = [...normalizedSymbols];
      if (!symbols.length || symbols.some((symbol) => !/^[A-Z0-9]{2,40}$/.test(symbol))) throw new Error("Укажите хотя бы один корректный символ, например BTCUSDT.");
      const instruments = symbols.map((symbol) => knownInstruments.find((instrument) => instrument.symbol === symbol));
      if (instruments.some((instrument) => !instrument)) throw new Error("Нужен каталог либо сохранённые полные идентификаторы выбранных инструментов. По одним символам offline-проверка невозможна.");
      if (algorithms.kind !== "ready" || !algorithms.data.algorithms.some((algorithm) => algorithm.id === form.algorithmId)) throw new Error("Выберите доступный алгоритм.");
      setChecking(true);
      const response = await apiFetch("/v1/replay-preflight", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ algorithmId: form.algorithmId, instruments: instruments.map((item) => ({ exchange: item!.exchange, category: item!.category, symbol: item!.symbol })), ...range }), signal: controller.signal });
      if (!response.ok) throw new Error(`Проверка данных недоступна: HTTP ${response.status}`);
      const result = await response.json() as PreflightResponse;
      if (!controller.signal.aborted) setPreview(result);
    } catch (cause) { if (!controller.signal.aborted) setError(errorText(cause)); }
    finally { if (!controller.signal.aborted) setChecking(false); }
  }
  async function launch() {
    if (!preview?.canRun) return;
    setBusy(true); setError(null); setMessage(null);
    try {
      await writePlatform("/v1/replay-runs", preview.request, "POST");
      setPreview(null); setMessage("Запрос запуска принят. Результаты и статус обновляются в списке ниже."); setReload((value) => value + 1);
    } catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }
  async function saveForm() {
    setBusy(true); setError(null); setMessage(null);
    try { await savePreference("replay", { symbols: [...normalizedSymbols].join("\n"), instruments: knownInstruments.filter((item) => normalizedSymbols.has(item.symbol)).map(({ exchange, category, symbol }) => ({ exchange, category, symbol })), algorithmId: form.algorithmId, ...replayRange(form.from, form.to) }); setMessage("Форма сохранена в базе сигналов."); }
    catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }
  async function cancelRun(id: number, isBatch: boolean) {
    setCancelling(id); setError(null); setMessage(null);
    try {
      await writePlatform(isBatch ? `/v1/replay-batches/${id}/cancel` : `/v1/replay-runs/${id}/cancel`, {}, "POST");
      setMessage(isBatch ? `Отмена запуска #${id} запрошена для всех инструментов.` : `Отмена запуска #${id} запрошена. Дождитесь конечного статуса.`); setReload((value) => value + 1);
    }
    catch (cause) { setError(errorText(cause)); }
    finally { setCancelling(null); }
  }
  function armPurge() {
    if (!purgeArmed) {
      setPurgeArmed(true);
      window.setTimeout(() => setPurgeArmed(false), 6000);
      return;
    }
    setPurgeArmed(false);
    void purgeAll();
  }
  async function purgeAll() {
    setPurging(true); setError(null); setMessage(null);
    try {
      for (let attempt = 0; attempt < 16; attempt += 1) {
        const response = await apiFetch("/v1/replay-runs/purge", { method: "POST", headers: { "content-type": "application/json" }, body: "{}" });
        const body = await response.json() as { runsDeleted?: number; cacheDeleted?: number; qualityIntervalsDeleted?: number; active?: number; queuedCancelled?: number };
        if (response.status === 409) {
          setMessage(`Останавливаю активные прогоны (${body.active ?? "?"}) и очищаю…`);
        } else if (!response.ok) {
          throw new Error(`Очистка недоступна: HTTP ${response.status}`);
        } else {
          setMessage(`Удалено: прогонов ${body.runsDeleted ?? 0}, записей кеша ${body.cacheDeleted ?? 0}, интервалов качества ${body.qualityIntervalsDeleted ?? 0}.`);
          setSelected(null); setPreview(null); setReload((value) => value + 1);
          return;
        }
        await new Promise((resolve) => window.setTimeout(resolve, 4000));
      }
      setError("Очистка не завершилась: активные прогоны слишком долго останавливались. Повторите позже.");
    } catch (cause) { setError(errorText(cause)); }
    finally { setPurging(false); }
  }
  async function confirmDeleteRun() {
    if (deleteTarget === null) return;
    setDeleting(true); setDeleteError(null);
    try {
      const response = await apiFetch(deleteTarget.isBatch ? `/v1/replay-batches/${deleteTarget.id}` : `/v1/replay-runs/${deleteTarget.id}`, { method: "DELETE" });
      if (!response.ok) throw new Error(response.status === 409 ? "Запуск ещё активен: сначала отмените его." : `Не удалось удалить запуск: HTTP ${response.status}`);
      setMessage(`Запуск #${deleteTarget.id} удалён.`);
      if (selected?.id === deleteTarget.id && selected.isBatch === deleteTarget.isBatch) setSelected(null);
      setDeleteTarget(null);
      setReload((value) => value + 1);
    } catch (cause) { setDeleteError(cause instanceof Error ? cause.message : "Ошибка соединения"); }
    finally { setDeleting(false); }
  }
  return <section aria-labelledby="runs-heading">
    <PageHeader id="runs-heading" title="Исторические запуски" />
    <form onSubmit={(event) => { void submit(event); }} className="mt-6">
      <Card className="grid gap-4">
        <Field label="Алгоритм">
          {(control) => <Select required disabled={busy || !ready || algorithms.kind !== "ready"} value={form.algorithmId} onChange={(event) => changeForm({ ...form, algorithmId: event.target.value })} {...control}>
            <option value="">Выберите алгоритм</option>
            {algorithms.kind === "ready" && algorithms.data.algorithms.map((algorithm) => <option key={algorithm.id} value={algorithm.id}>{algorithm.name} · {algorithm.version}</option>)}
          </Select>}
        </Field>
        {algorithms.kind === "unreachable" && <Alert variant="destructive" role="alert">Алгоритмы недоступны ({algorithms.detail})</Alert>}
        <p className="text-sm text-muted-foreground">Запуск использует сохранённый снимок общих параметров. <Link className="font-medium text-primary underline underline-offset-2" to="/settings">Настроить алгоритмы</Link></p>
        {catalog.kind === "loading" && <p className="text-sm text-muted-foreground">Загрузка инструментов из БД…</p>}
        {catalog.kind === "unreachable" && <Alert variant="warning" role="status">Каталог БД недоступен ({catalog.detail}). Проверка кеша возможна только для ранее сохранённых полных идентификаторов: {knownInstruments.map((item) => `${item.exchange}/${item.category}/${item.symbol}`).join(", ") || "не сохранены"}. Выбор: {form.symbols || "пусто"}.</Alert>}
        {catalog.kind === "ready" && <InstrumentPicker catalog={catalog.data.instruments} symbols={form.symbols} onChange={(symbols) => changeForm({ ...form, symbols })} disabled={!ready || busy} limit={null} />}
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Начало">{(control) => <Input type="datetime-local" required disabled={!ready || busy} value={form.from} onChange={(event) => changeForm({ ...form, from: utcMidnightOnDateChange(form.from, event.target.value) })} {...control} />}</Field>
          <Field label="Конец">{(control) => <Input type="datetime-local" required disabled={!ready || busy} value={form.to} onChange={(event) => changeForm({ ...form, to: utcMidnightOnDateChange(form.to, event.target.value) })} {...control} />}</Field>
        </div>
        <p className="text-xs text-muted-foreground">Локальное время: {localTimezoneLabel()}. В API передаются Unix-секунды.</p>
        {error && <Alert variant="destructive" role="alert">{error}</Alert>}
        {message && <Alert variant="success" role="status">{message}</Alert>}
        <p className="text-xs text-muted-foreground">Расчёт выполняется в фоне. Список обновляется каждые 3 секунды в видимой вкладке. Закрытие страницы не отменяет расчёт; используйте кнопку отмены.</p>
        <div className="flex flex-wrap gap-2"><Button type="submit" disabled={busy || checking || !ready || algorithms.kind !== "ready"}>{checking ? "Проверка…" : "Проверить данные"}</Button><Button type="button" variant="outline" disabled={busy || checking || !ready} onClick={() => { void saveForm(); }}>Сохранить форму</Button></div>
        {preview && <><ReplayPreflight preview={preview} /><Button type="button" disabled={busy || !preview.canRun} onClick={() => { void launch(); }}>Запустить проверенный расчёт</Button></>}
      </Card>
    </form>
    <div className="mt-10"><SectionHeader title="Последние запуски" actions={<>
      <Button variant="outline" size="sm" onClick={() => setReload((value) => value + 1)}>Обновить</Button>
      <Button variant="outline" size="sm" disabled={busy || checking || purging} onClick={armPurge}>{purging ? "Очищаю…" : purgeArmed ? "Подтвердить очистку всего" : "Очистить все прогоны"}</Button>
    </>} /></div>
    {runs.kind === "loading" && <p className="mt-4 text-sm text-muted-foreground">Загрузка…</p>}
    {runs.kind === "unreachable" && <Alert className="mt-4" variant="destructive" role="alert">Запуски недоступны ({runs.detail})</Alert>}
    {runs.kind === "ready" && <div className="mt-4 overflow-x-auto">
      <Table>
        <caption className="text-left text-muted-foreground">До 200 последних запусков. Время: {localTimezoneLabel()}</caption>
        <TableHeader><TableRow><TableHead>ID</TableHead><TableHead>Имя</TableHead><TableHead>Инструмент</TableHead><TableHead>Алгоритм</TableHead><TableHead>Статус</TableHead><TableHead>Кеш</TableHead><TableHead className="text-center">Сетапов</TableHead><TableHead className="text-center">Действие</TableHead></TableRow></TableHeader>
        <TableBody>{runs.data.runs.slice(0, 200).map((run) => <TableRow key={`${run.isBatch === true ? "batch" : "run"}-${run.id}`}>
          <TableCell><Button variant="outline" size="sm" onClick={() => setSelected({ id: run.id, isBatch: run.isBatch === true })} aria-label={`Результаты запуска ${run.id}`}>#{run.id}</Button></TableCell>
          <TableCell><RunNameEditor run={run} onSaved={() => setReload((value) => value + 1)} /></TableCell>
          <TableCell className="font-medium">{run.symbol}</TableCell>
          <TableCell className="text-muted-foreground">{run.algorithmId} · {run.algorithmVersion}</TableCell>
          <TableCell><Badge variant={runStatusVariant(run.status)}>{RUN_STATUS_LABELS[run.status] ?? run.status}{run.isBatch === true && run.status === "running" ? ` (${run.doneInstruments ?? 0}/${run.instrumentCount ?? 0})` : ""}</Badge><p className="mt-1 text-xs text-muted-foreground">{qualityLabel(run.quality)}</p></TableCell>
          <TableCell>{run.cacheHit ? "да" : "нет"}</TableCell>
          <TableCell numeric className="text-center">{run.setupCount}</TableCell>
              <TableCell>
                <div className="flex items-center justify-center gap-1">
                  {["queued", "running", "cancel_requested"].includes(run.status) && <Button variant="outline" size="sm" disabled={cancelling !== null || run.status === "cancel_requested"} onClick={() => { void cancelRun(run.id, run.isBatch === true); }} aria-label={`Отменить запуск ${run.id}`}>Отменить</Button>}
                  {!["queued", "running", "cancel_requested"].includes(run.status) && <Button variant="outline" size="sm" disabled={deleting} aria-label={`Удалить запуск ${run.id}`} onClick={() => { setDeleteError(null); setDeleteTarget({ id: run.id, symbol: run.symbol, isBatch: run.isBatch === true }); }}><TrashIcon /></Button>}
                </div>
              </TableCell>
        </TableRow>)}</TableBody>
      </Table>
      {runs.data.runs.length === 0 && <p className="mt-2 text-sm text-muted-foreground">Запусков пока нет.</p>}
    </div>}
      {selected !== null && <RunResults key={`${selected.isBatch ? "b" : "r"}-${selected.id}`} id={selected.id} isBatch={selected.isBatch} revision={reload} />}
      {deleteTarget !== null && (
        <Dialog open onOpenChange={(open) => { if (!open && !deleting) setDeleteTarget(null); }}>
          <DialogContent>
            <DialogHeader>
              <DialogTitle>Удалить запуск #{deleteTarget.id}?</DialogTitle>
              <DialogDescription>{deleteTarget.isBatch
                ? `Пакетный запуск #${deleteTarget.id} (${deleteTarget.symbol}) будет полностью удалён из базы вместе со всеми дочерними прогонами и их интервалами качества. Кеш вычислений не затрагивается. Действие необратимо.`
                : `Запуск ${deleteTarget.symbol} и связанные интервалы качества будут полностью удалены из базы. Кеш вычислений не затрагивается. Действие необратимо.`}</DialogDescription>
            </DialogHeader>
            {deleteError && <Alert variant="destructive" role="alert">{deleteError}</Alert>}
            <div className="flex justify-end gap-2">
              <Button variant="outline" disabled={deleting} onClick={() => setDeleteTarget(null)}>Отмена</Button>
              <Button disabled={deleting} onClick={() => { void confirmDeleteRun(); }}>{deleting ? "Удаление…" : "Удалить"}</Button>
            </div>
          </DialogContent>
        </Dialog>
      )}
  </section>;
}
