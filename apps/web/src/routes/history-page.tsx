import { useState, type FormEvent } from "react";
import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import { Select } from "../components/ui/select";
import { type CatalogInstrument } from "./instrument-picker";
import { errorText, localTime, localTimezone, replayRange, writePlatform } from "./platform-controls";
import { usePlatformPoll } from "./use-platform-poll";

interface Coverage { instrumentId: number; firstMinuteUnix: number | null; lastMinuteUnix: number | null; totalCandles: number; intervals: { startUnix: number; endUnix: number }[]; gaps: { startUnix: number; endUnix: number }[] }
interface BackfillJob { id: string | number; instrumentId: number; fromUnix: number; toUnix: number; status: string; checkpointUnix: number | null; error: string | null }
interface CollectionTarget { instrumentId: number; enabled: boolean; lastCollectedUnix: number | null }

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

export function HistoryPage() {
  const [revision, setRevision] = useState(0);
  const catalog = usePlatformPoll<{ instruments: CatalogInstrument[] }>("/v1/instruments", revision, 0);
  const jobs = usePlatformPoll<{ jobs: BackfillJob[] }>("/v1/history/backfill-jobs", revision);
  const targets = usePlatformPoll<{ targets: CollectionTarget[] }>("/v1/history/collection-targets", revision);
  const [instrumentId, setInstrumentId] = useState("");
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const selected = catalog.kind === "ready" ? catalog.data.instruments.find((instrument) => String(instrument.id) === instrumentId) : undefined;
  const target = targets.kind === "ready" ? targets.data.targets.find((item) => item.instrumentId === selected?.id) : undefined;
  async function mutation(path: string, body: unknown, method: string, success: string) {
    setBusy(true); setError(null); setMessage(null);
    try { await writePlatform(path, body, method); setMessage(success); setRevision((value) => value + 1); }
    catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }
  function enqueue(event: FormEvent<HTMLFormElement>) {
    event.preventDefault(); setError(null); setMessage(null);
    try {
      if (!selected) throw new Error("Выберите инструмент из каталога.");
      const range = replayRange(from, to);
      void mutation("/v1/history/backfill-jobs", { instrumentId: selected.id, ...range }, "POST", "Запрос загрузки истории принят. Проверяйте статус задания ниже.");
    } catch (cause) { setError(errorText(cause)); }
  }
  const instrumentName = (id: number) => catalog.kind === "ready" ? catalog.data.instruments.find((instrument) => instrument.id === id)?.symbol ?? `ID ${id}` : `ID ${id}`;
  return <section aria-labelledby="history-heading" className="grid gap-5">
    <h1 id="history-heading" className="text-2xl font-semibold">История и сбор</h1>
    <p className="text-sm text-muted-foreground">Покрытие, историческая загрузка и текущий сбор независимы от replay и live-сканера. Действия выполняются только по вашей команде. Время: {localTimezone}.</p>
    <div className="flex flex-wrap items-end gap-3"><label className="grid min-w-0 gap-1 text-sm">Инструмент БД<Select value={instrumentId} disabled={catalog.kind !== "ready" || busy} onChange={(event) => setInstrumentId(event.target.value)}><option value="">Выберите инструмент</option>{catalog.kind === "ready" && catalog.data.instruments.map((instrument) => <option key={instrument.id} value={instrument.id}>{instrument.exchange} / {instrument.category} / {instrument.symbol} ({instrument.status})</option>)}</Select></label><Button variant="outline" disabled={busy} onClick={() => setRevision((value) => value + 1)}>Обновить историю</Button></div>
    {catalog.kind === "loading" && <p>Загрузка каталога…</p>}{catalog.kind === "unreachable" && <p role="alert">Каталог недоступен ({catalog.detail})</p>}
    {selected && <InstrumentCoverage key={selected.id} id={selected.id} revision={revision} />}
    <form className="grid max-w-2xl gap-3 rounded-md border border-input p-4" onSubmit={enqueue}>
      <h2 className="text-lg font-semibold">Загрузить историю</h2>
      <div className="grid gap-3 sm:grid-cols-2"><label className="grid gap-1 text-sm">Начало загрузки<Input type="datetime-local" required value={from} onChange={(event) => setFrom(event.target.value)} /></label><label className="grid gap-1 text-sm">Конец загрузки<Input type="datetime-local" required value={to} onChange={(event) => setTo(event.target.value)} /></label></div>
      <Button type="submit" disabled={busy || !selected}>Поставить загрузку в очередь</Button>
    </form>
    {error && <p role="alert">{error}</p>}{message && <p role="status">{message}</p>}
    <section className="grid gap-3" aria-labelledby="collection-heading"><h2 id="collection-heading" className="text-lg font-semibold">Текущий сбор свечей</h2>
      {targets.kind === "loading" && <p>Загрузка состояния сбора…</p>}{targets.kind === "unreachable" && <p role="alert">Состояние сбора неизвестно ({targets.detail}). Управление недоступно.</p>}
      {targets.kind === "ready" && <><p>{selected ? `Выбранный инструмент: ${target?.enabled ? "сбор включён" : "сбор выключен"}.` : "Выберите инструмент для управления сбором."}</p><Button variant="outline" disabled={busy || !selected} onClick={() => { if (selected) void mutation(`/v1/history/collection-targets/${selected.id}`, { enabled: !target?.enabled }, "PUT", "Настройка сбора сохранена."); }}>{target?.enabled ? "Отключить сбор" : "Включить сбор"}</Button>
        <div className="overflow-x-auto"><table className="w-full text-sm"><caption className="text-left text-muted-foreground">Первые 100 целей сбора из ответа API</caption><thead><tr className="text-left"><th className="p-2">Инструмент</th><th className="p-2">Включён</th><th className="p-2">Последняя собранная минута</th></tr></thead><tbody>{targets.data.targets.slice(0, 100).map((item) => <tr key={item.instrumentId} className="border-b border-input"><td className="p-2">{instrumentName(item.instrumentId)}</td><td className="p-2">{item.enabled ? "да" : "нет"}</td><td className="p-2">{item.lastCollectedUnix === null ? "нет данных" : localTime(item.lastCollectedUnix)}</td></tr>)}</tbody></table>{targets.data.targets.length === 0 && <p>Цели сбора ещё не настроены.</p>}</div></>}
    </section>
    <section className="grid gap-3" aria-labelledby="backfill-heading"><h2 id="backfill-heading" className="text-lg font-semibold">Задания загрузки</h2><p className="text-xs text-muted-foreground">Обновление каждые 3 секунды в видимой вкладке. Прогресс отражает checkpoint диапазона, не полноту свечей.</p>
      {jobs.kind === "loading" && <p>Загрузка заданий…</p>}{jobs.kind === "unreachable" && <p role="alert">Задания недоступны ({jobs.detail})</p>}
      {jobs.kind === "ready" && <div className="overflow-x-auto"><table className="w-full text-sm"><caption className="text-left text-muted-foreground">Первые 100 заданий из ответа API</caption><thead><tr className="text-left"><th className="p-2">ID / инструмент</th><th className="p-2">Диапазон</th><th className="p-2">Статус</th><th className="p-2">Checkpoint</th><th className="p-2">Действие</th></tr></thead><tbody>{jobs.data.jobs.slice(0, 100).map((job) => <tr key={job.id} className="border-b border-input"><td className="p-2">#{job.id} · {instrumentName(job.instrumentId)}</td><td className="p-2">{localTime(job.fromUnix)} – {localTime(job.toUnix)}</td><td className="p-2">{job.status}{job.error && <p className="max-w-sm break-words" role="alert">{job.error}</p>}</td><td className="p-2">{job.checkpointUnix === null ? "ещё нет" : `${localTime(job.checkpointUnix)} (${Math.round(Math.max(0, Math.min(1, (job.checkpointUnix - job.fromUnix) / Math.max(1, job.toUnix - job.fromUnix))) * 100)}%)`}</td><td className="p-2">{["queued", "running", "cancel_requested"].includes(job.status) && <Button variant="outline" disabled={busy || job.status === "cancel_requested"} onClick={() => { void mutation(`/v1/history/backfill-jobs/${encodeURIComponent(String(job.id))}/cancel`, {}, "POST", `Отмена задания #${job.id} запрошена.`); }}>Отменить загрузку {job.id}</Button>}</td></tr>)}</tbody></table>{jobs.data.jobs.length === 0 && <p>Заданий загрузки пока нет.</p>}</div>}
    </section>
  </section>;
}
