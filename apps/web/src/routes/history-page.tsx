import { useState, type FormEvent } from "react";
import { apiFetch } from "../lib/platform-api";
import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import { Select } from "../components/ui/select";
import { InstrumentPicker, type CatalogInstrument } from "./instrument-picker";
import { errorText, localTime, localTimezone, replayRange, utcMidnightOnDateChange, writePlatform } from "./platform-controls";
import { usePlatformPoll } from "./use-platform-poll";

interface Coverage { instrumentId: number; firstMinuteUnix: number | null; lastMinuteUnix: number | null; totalCandles: number; intervals: { startUnix: number; endUnix: number }[]; gaps: { startUnix: number; endUnix: number }[] }
interface BackfillJob { id: string | number; instrumentId: number; fromUnix: number; toUnix: number; status: string; checkpointUnix: number | null; error: string | null }
interface CollectionTarget { instrumentId: number; enabled: boolean; lastCollectedUnix: number | null }
interface JobCounts { queued: number; running: number; done: number; cancel_requested: number; cancelled: number; failed: number }

const HISTORY_TABS = [
  { id: "upload", label: "Загрузка истории" },
  { id: "collection", label: "Текущий сбор свечей" },
  { id: "jobs", label: "Задания загрузки" },
] as const;
type HistoryTab = (typeof HISTORY_TABS)[number]["id"];

function InstrumentCoverage({ id, revision }: { id: number; revision: number }) {
  const state = usePlatformPoll<Coverage>(`/v1/history/instruments/${id}/coverage`, revision, 0);
  if (state.kind === "loading") return <p>Загрузка покрытия…</p>;
  if (state.kind === "unreachable") return <p role="alert">Покрытие неизвестно: API недоступен ({state.detail}). Это не означает отсутствие данных.</p>;
  return <section className="grid gap-3" aria-labelledby="coverage-heading"><h2 id="coverage-heading" className="text-lg font-semibold">Покрытие истории</h2>
    <p>Свечей: {state.data.totalCandles}. Первая минута: {state.data.firstMinuteUnix === null ? "нет" : localTime(state.data.firstMinuteUnix)}. Последняя минута: {state.data.lastMinuteUnix === null ? "нет" : localTime(state.data.lastMinuteUnix)}.</p>
    {state.data.totalCandles === 0 && <p>В БД нет свечей этого инструмента.</p>}
    <div className="grid gap-3 sm:grid-cols-2">{[{ label: "Непрерывные интервалы", rows: state.data.intervals }, { label: "Пропуски", rows: state.data.gaps }].map((group) => <div key={group.label} className="rounded-md border border-input p-3"><h3 className="font-medium">{group.label}: {group.rows.length}</h3><ul className="mt-2 max-h-64 overflow-y-auto text-sm">{group.rows.slice(0, 100).map((interval, index) => <li key={index}>{localTime(interval.startUnix)} – {localTime(interval.endUnix)}</li>)}</ul>{group.rows.length > 100 && <p className="text-sm text-muted-foreground">Показаны первые 100.</p>}</div>)}</div>
  </section>;
}

interface CoverageSummaryRow { instrumentId: number; exchange: string; category: string; symbol: string; status: string; firstMinuteUnix: number | null; lastMinuteUnix: number | null; totalCandles: number; missingMinutes: number }

function CoverageOverview({ revision }: { revision: number }) {
  const state = usePlatformPoll<{ instruments: CoverageSummaryRow[] }>("/v1/history/instruments/coverage-summary", revision, 0);
  const [filter, setFilter] = useState("");
  const [withDataOnly, setWithDataOnly] = useState(true);
  if (state.kind === "loading") return <p>Загрузка обзора покрытия…</p>;
  if (state.kind === "unreachable") return <p role="alert">Обзор покрытия неизвестен: API недоступен ({state.detail}). Это не означает отсутствие данных.</p>;
  const all = state.data.instruments;
  const withData = all.filter((row) => row.totalCandles > 0).length;
  const needle = filter.trim().toLowerCase();
  const rows = all.filter((row) => (!withDataOnly || row.totalCandles > 0) && row.symbol.toLowerCase().includes(needle));
  return <section aria-labelledby="coverage-overview-heading" className="grid gap-3">
    <h2 id="coverage-overview-heading" className="text-lg font-semibold">Обзор покрытия истории</h2>
    <p className="text-xs text-muted-foreground">Инструментов со свечами: {withData} из {all.length}. «Пропущено минут» — суммарные разрывы между первой и последней сохранённой минутой инструмента; точные интервалы — в детальном покрытии ниже.</p>
    <div className="flex flex-wrap items-end gap-3">
      <label className="grid gap-1 text-sm">Фильтр по символу<Input value={filter} onChange={(event) => setFilter(event.target.value)} /></label>
      <label className="flex items-center gap-2 pb-1 text-sm"><input type="checkbox" checked={withDataOnly} onChange={(event) => setWithDataOnly(event.target.checked)} /> Только инструменты со свечами</label>
    </div>
    <div className="max-h-96 overflow-auto rounded-md border border-input"><table className="w-full text-sm"><thead className="sticky top-0 bg-background"><tr className="text-left"><th className="p-2">Инструмент</th><th className="p-2">Статус</th><th className="p-2">Первая минута</th><th className="p-2">Последняя минута</th><th className="p-2">Свечей</th><th className="p-2">Пропущено минут</th></tr></thead><tbody>{rows.slice(0, 200).map((row) => <tr key={row.instrumentId} className="border-b border-input"><td className="p-2">{row.symbol}</td><td className="p-2">{row.status}</td><td className="p-2">{row.firstMinuteUnix === null ? "нет" : localTime(row.firstMinuteUnix)}</td><td className="p-2">{row.lastMinuteUnix === null ? "нет" : localTime(row.lastMinuteUnix)}</td><td className="p-2">{row.totalCandles}</td><td className="p-2">{row.totalCandles === 0 ? "—" : row.missingMinutes}</td></tr>)}</tbody></table></div>
    {rows.length > 200 && <p className="text-sm text-muted-foreground">Показаны первые 200 из {rows.length}.</p>}
    {rows.length === 0 && <p>Нет инструментов по текущему фильтру.</p>}
  </section>;
}

export function HistoryPage() {
  const [revision, setRevision] = useState(0);
  const [tab, setTab] = useState<HistoryTab>("upload");
  const catalog = usePlatformPoll<{ instruments: CatalogInstrument[] }>("/v1/instruments", revision, 0);
  const jobs = usePlatformPoll<{ jobs: BackfillJob[]; counts: JobCounts }>("/v1/history/backfill-jobs", revision);
  const targets = usePlatformPoll<{ targets: CollectionTarget[] }>("/v1/history/collection-targets", revision);
  const [instrumentId, setInstrumentId] = useState("");
  const [symbols, setSymbols] = useState("");
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const instruments = catalog.kind === "ready" ? catalog.data.instruments : [];
  const selected = instruments.find((instrument) => String(instrument.id) === instrumentId);
  const target = targets.kind === "ready" ? targets.data.targets.find((item) => item.instrumentId === selected?.id) : undefined;
  const pickedInstruments = [...new Set(symbols.split(/[\s,;]+/).filter(Boolean))]
    .map((symbol) => instruments.find((instrument) => instrument.symbol === symbol))
    .filter((instrument): instrument is CatalogInstrument => instrument !== undefined);
  const counts = jobs.kind === "ready" ? jobs.data.counts : null;
  const activeJobs = counts === null ? 0 : counts.queued + counts.running + counts.cancel_requested;
  const instrumentName = (id: number) => instruments.find((instrument) => instrument.id === id)?.symbol ?? `ID ${id}`;

  async function importListing() {
    setBusy(true); setError(null); setMessage(null);
    try {
      const response = await apiFetch("/v1/history/instruments/listing");
      const payload = await response.json() as { instruments?: CatalogInstrument[]; error?: string };
      if (!response.ok) throw new Error(payload.error ?? `HTTP ${response.status}`);
      const listing = payload.instruments ?? [];
      const merged = new Set([...pickedInstruments.map((instrument) => instrument.symbol), ...listing.map((instrument) => instrument.symbol)]);
      setSymbols([...merged].sort().join("\n"));
      setRevision((value) => value + 1);
      setMessage(`Импортирован текущий листинг Bybit: ${listing.length} линейных бессрочных USDT-контрактов. Список можно отредактировать перед подтверждением загрузки.`);
    } catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }

  async function mutation(path: string, body: unknown, method: string, success: string) {
    setBusy(true); setError(null); setMessage(null);
    try { await writePlatform(path, body, method); setMessage(success); setRevision((value) => value + 1); }
    catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }

  async function enqueue(event: FormEvent<HTMLFormElement>) {
    event.preventDefault(); setError(null); setMessage(null);
    try {
      if (pickedInstruments.length === 0) throw new Error("Выберите хотя бы один инструмент: вручную, импортом CSV или текущим листингом.");
      const range = replayRange(from, to);
      setBusy(true);
      const response = await apiFetch("/v1/history/backfill-jobs/batch", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ instrumentIds: pickedInstruments.map((instrument) => instrument.id), ...range }),
      });
      const payload = await response.json() as { created?: number; declined?: number; error?: string };
      if (!response.ok) throw new Error(payload.error ?? `HTTP ${response.status}`);
      setRevision((value) => value + 1);
      setMessage((payload.declined ?? 0) > 0
        ? `Создано заданий: ${payload.created}. Отклонено по ресурсным лимитам: ${payload.declined}. Остальные инструменты можно догрузить позже, когда очередь освободится.`
        : `Создано заданий: ${payload.created}. Прогресс и отмена — на вкладке «Задания загрузки».`);
    } catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }

  return <section aria-labelledby="history-heading" className="grid gap-5">
    <h1 id="history-heading" className="text-2xl font-semibold">История и сбор</h1>
    <p className="text-sm text-muted-foreground">Загрузка истории и текущий сбор независимы от replay и live-сканера. Действия выполняются только по вашей команде. Время: {localTimezone}.</p>
    {jobs.kind === "unreachable" && <p role="alert" className="text-sm">Статус массовых загрузок неизвестен ({jobs.detail}). Это не означает, что загрузок нет.</p>}
    {counts !== null && (activeJobs > 0
      ? <p role="status" className="rounded-md border border-input p-3 text-sm">Идёт массовая загрузка истории: активных заданий {activeJobs} (в очереди {counts.queued}, выполняется {counts.running}, запрошена отмена {counts.cancel_requested}). Статус хранится на сервере и виден после перезагрузки страницы.</p>
      : <p role="status" className="text-sm text-muted-foreground">Массовых загрузок, запущенных пользователем, сейчас нет. Постоянный ежеминутный сбор закрытых минут работает отдельно и здесь не учитывается.</p>)}
    <div role="tablist" aria-label="Разделы «История и сбор»" className="flex flex-wrap gap-2">
      {HISTORY_TABS.map((item) => <Button key={item.id} type="button" variant={tab === item.id ? "default" : "outline"} role="tab" aria-selected={tab === item.id} onClick={() => setTab(item.id)}>{item.label}</Button>)}
    </div>
    {error && <p role="alert">{error}</p>}{message && <p role="status">{message}</p>}

    {tab === "upload" && <section className="grid gap-3" aria-labelledby="upload-heading">
      <h2 id="upload-heading" className="text-lg font-semibold">Загрузить историю</h2>
      <p className="text-xs text-muted-foreground">Выберите инструменты вручную, импортом CSV без заголовка или текущим листингом Bybit, при необходимости отредактируйте список и подтвердите загрузку для выбранных дат. Создаётся отдельное задание на каждый инструмент.</p>
      <div className="flex flex-wrap gap-2">
        <Button type="button" variant="outline" disabled={busy} onClick={() => { void importListing(); }}>Импортировать текущий листинг Bybit (линейные USDT perpetual)</Button>
        <Button type="button" variant="outline" disabled={busy || symbols.trim().length === 0} onClick={() => { setSymbols(""); setMessage(null); }}>Очистить выбор</Button>
      </div>
      <InstrumentPicker catalog={instruments} symbols={symbols} onChange={(value) => { setSymbols(value); setMessage(null); }} disabled={busy || catalog.kind !== "ready"} />
      <form className="grid max-w-2xl gap-3 rounded-md border border-input p-4" onSubmit={(event) => { void enqueue(event); }}>
        <div className="grid gap-3 sm:grid-cols-2"><label className="grid gap-1 text-sm">Начало загрузки<Input type="datetime-local" required value={from} onChange={(event) => setFrom(utcMidnightOnDateChange(from, event.target.value))} /></label><label className="grid gap-1 text-sm">Конец загрузки<Input type="datetime-local" required value={to} onChange={(event) => setTo(utcMidnightOnDateChange(to, event.target.value))} /></label></div>
        <p className="text-sm text-muted-foreground">Выбрано инструментов: {pickedInstruments.length}.</p>
        <Button type="submit" disabled={busy || pickedInstruments.length === 0}>Поставить загрузку в очередь ({pickedInstruments.length})</Button>
      </form>
    </section>}

    {tab === "collection" && <section className="grid gap-3" aria-labelledby="collection-heading">
      <h2 id="collection-heading" className="text-lg font-semibold">Текущий сбор свечей</h2>
      <CoverageOverview revision={revision} />
      <div className="flex flex-wrap items-end gap-3"><label className="grid min-w-0 gap-1 text-sm">Инструмент БД<Select value={instrumentId} disabled={catalog.kind !== "ready" || busy} onChange={(event) => setInstrumentId(event.target.value)}><option value="">Выберите инструмент</option>{instruments.map((instrument) => <option key={instrument.id} value={instrument.id}>{instrument.exchange} / {instrument.category} / {instrument.symbol} ({instrument.status})</option>)}</Select></label><Button variant="outline" disabled={busy} onClick={() => setRevision((value) => value + 1)}>Обновить историю</Button></div>
      {catalog.kind === "loading" && <p>Загрузка каталога…</p>}{catalog.kind === "unreachable" && <p role="alert">Каталог недоступен ({catalog.detail})</p>}
      {selected && <InstrumentCoverage key={selected.id} id={selected.id} revision={revision} />}
      {targets.kind === "loading" && <p>Загрузка состояния сбора…</p>}{targets.kind === "unreachable" && <p role="alert">Состояние сбора неизвестно ({targets.detail}). Управление недоступно.</p>}
      {targets.kind === "ready" && <><p>{selected ? `Выбранный инструмент: ${target?.enabled ? "сбор включён" : "сбор выключен"}.` : "Выберите инструмент для управления сбором."}</p><Button variant="outline" disabled={busy || !selected} onClick={() => { if (selected) void mutation(`/v1/history/collection-targets/${selected.id}`, { enabled: !target?.enabled }, "PUT", "Настройка сбора сохранена."); }}>{target?.enabled ? "Отключить сбор" : "Включить сбор"}</Button>
        <div className="overflow-x-auto"><table className="w-full text-sm"><caption className="text-left text-muted-foreground">Первые 100 целей сбора из ответа API</caption><thead><tr className="text-left"><th className="p-2">Инструмент</th><th className="p-2">Включён</th><th className="p-2">Последняя собранная минута</th></tr></thead><tbody>{targets.data.targets.slice(0, 100).map((item) => <tr key={item.instrumentId} className="border-b border-input"><td className="p-2">{instrumentName(item.instrumentId)}</td><td className="p-2">{item.enabled ? "да" : "нет"}</td><td className="p-2">{item.lastCollectedUnix === null ? "нет данных" : localTime(item.lastCollectedUnix)}</td></tr>)}</tbody></table>{targets.data.targets.length === 0 && <p>Цели сбора ещё не настроены.</p>}</div></>}
    </section>}

    {tab === "jobs" && <section className="grid gap-3" aria-labelledby="backfill-heading">
      <h2 id="backfill-heading" className="text-lg font-semibold">Задания загрузки</h2><p className="text-xs text-muted-foreground">Обновление каждые 3 секунды в видимой вкладке. Прогресс отражает checkpoint диапазона, не полноту свечей.</p>
      {counts !== null && <p className="text-sm text-muted-foreground">Активных: {activeJobs}. Завершено успешно: {counts.done}. С ошибкой: {counts.failed}. Отменено: {counts.cancelled}.</p>}
      {jobs.kind === "loading" && <p>Загрузка заданий…</p>}{jobs.kind === "unreachable" && <p role="alert">Задания недоступны ({jobs.detail})</p>}
      {jobs.kind === "ready" && <div className="overflow-x-auto"><table className="w-full text-sm"><caption className="text-left text-muted-foreground">Первые 100 заданий загрузки из ответа API</caption><thead><tr className="text-left"><th className="p-2">ID / инструмент</th><th className="p-2">Диапазон</th><th className="p-2">Статус</th><th className="p-2">Checkpoint</th><th className="p-2">Действие</th></tr></thead><tbody>{jobs.data.jobs.slice(0, 100).map((job) => <tr key={job.id} className="border-b border-input"><td className="p-2">#{job.id} · {instrumentName(job.instrumentId)}</td><td className="p-2">{localTime(job.fromUnix)} – {localTime(job.toUnix)}</td><td className="p-2">{job.status}{job.error && <p className="max-w-sm break-words" role="alert">{job.error}</p>}</td><td className="p-2">{job.checkpointUnix === null ? "ещё нет" : `${localTime(job.checkpointUnix)} (${Math.round(Math.max(0, Math.min(1, (job.checkpointUnix - job.fromUnix) / Math.max(1, job.toUnix - job.fromUnix))) * 100)}%)`}</td><td className="p-2">{["queued", "running", "cancel_requested"].includes(job.status) && <Button variant="outline" disabled={busy || job.status === "cancel_requested"} onClick={() => { void mutation(`/v1/history/backfill-jobs/${encodeURIComponent(String(job.id))}/cancel`, {}, "POST", `Отмена задания #${job.id} запрошена.`); }}>Отменить загрузку {job.id}</Button>}</td></tr>)}</tbody></table>{jobs.data.jobs.length === 0 && <p>Заданий загрузки пока нет.</p>}</div>}
    </section>}
  </section>;
}
