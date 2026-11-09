import { useState, type FormEvent } from "react";
import { apiFetch } from "../lib/platform-api";
import { Alert } from "../components/ui/alert";
import { Badge } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import { Card } from "../components/ui/card";
import { Checkbox } from "../components/ui/checkbox";
import { Field } from "../components/ui/field";
import { Input } from "../components/ui/input";
import { PageHeader, SectionHeader } from "../components/ui/page-header";
import { SegmentedList, SegmentedTrigger } from "../components/ui/segmented";
import { Select } from "../components/ui/select";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "../components/ui/table";
import { InstrumentPicker, type CatalogInstrument } from "./instrument-picker";
import { errorText, localTime, localTimezoneLabel, replayRange, utcMidnightOnDateChange, writePlatform } from "./platform-controls";
import { usePlatformPoll } from "./use-platform-poll";

interface Coverage { instrumentId: number; firstMinuteUnix: number | null; lastMinuteUnix: number | null; totalCandles: number; intervals: { startUnix: number; endUnix: number }[]; gaps: { startUnix: number; endUnix: number }[] }
interface BackfillJob { id: string | number; instrumentId: number; fromUnix: number; toUnix: number; status: string; checkpointUnix: number | null; error: string | null }
interface CollectionTarget { instrumentId: number; enabled: boolean; lastCollectedUnix: number | null }
interface JobCounts { queued: number; running: number; done: number; cancel_requested: number; cancelled: number; failed: number }
interface BackfillPlanRow { id: number; fromUnix: number; toUnix: number; status: "active" | "completed" | "cancelled"; counts: { total: number; pending: number; enqueued: number; failed: number; cancelled: number }; activeJobs: number; createdAt: string; updatedAt: string }

const HISTORY_TABS = [
  { id: "upload", label: "Загрузка истории" },
  { id: "collection", label: "Текущий сбор свечей" },
  { id: "jobs", label: "Задания загрузки" },
] as const;
type HistoryTab = (typeof HISTORY_TABS)[number]["id"];

function InstrumentCoverage({ id, revision }: { id: number; revision: number }) {
  const state = usePlatformPoll<Coverage>(`/v1/history/instruments/${id}/coverage`, revision, 0);
  if (state.kind === "loading") return <p className="text-sm text-muted-foreground">Загрузка покрытия…</p>;
  if (state.kind === "unreachable") return <Alert variant="destructive" role="alert">Покрытие неизвестно: API недоступен ({state.detail}). Это не означает отсутствие данных.</Alert>;
  return <section className="grid gap-3" aria-labelledby="coverage-heading"><SectionHeader id="coverage-heading" title="Покрытие истории" />
    <p>Свечей: {state.data.totalCandles}. Первая минута: {state.data.firstMinuteUnix === null ? "нет" : localTime(state.data.firstMinuteUnix)}. Последняя минута: {state.data.lastMinuteUnix === null ? "нет" : localTime(state.data.lastMinuteUnix)}.</p>
    {state.data.totalCandles === 0 && <p>В БД нет свечей этого инструмента.</p>}
    <div className="grid gap-3 sm:grid-cols-2">{[{ label: "Непрерывные интервалы", rows: state.data.intervals }, { label: "Пропуски", rows: state.data.gaps }].map((group) => <Card key={group.label} className="gap-2 p-4"><h3 className="font-medium">{group.label}: {group.rows.length}</h3><ul className="max-h-64 overflow-y-auto text-sm">{group.rows.slice(0, 100).map((interval, index) => <li key={index}>{localTime(interval.startUnix)} – {localTime(interval.endUnix)}</li>)}</ul>{group.rows.length > 100 && <p className="text-sm text-muted-foreground">Показаны первые 100.</p>}</Card>)}</div>
  </section>;
}

interface CoverageSummaryRow { instrumentId: number; exchange: string; category: string; symbol: string; status: string; firstMinuteUnix: number | null; lastMinuteUnix: number | null; totalCandles: number; missingMinutes: number; verifiedMinutes: number }
interface GapRepairRow { id: number; instrumentId: number; symbol: string; status: "queued" | "running" | "done" | "failed"; gapsProcessed: number; filledMinutes: number; verifiedMinutes: number; error: string | null; createdAt: string; updatedAt: string }

function CoverageOverview({ revision }: { revision: number }) {
  const state = usePlatformPoll<{ instruments: CoverageSummaryRow[] }>("/v1/history/instruments/coverage-summary", revision, 0);
  const [filter, setFilter] = useState("");
  const [withDataOnly, setWithDataOnly] = useState(true);
  if (state.kind === "loading") return <p className="text-sm text-muted-foreground">Загрузка обзора покрытия…</p>;
  if (state.kind === "unreachable") return <Alert variant="destructive" role="alert">Обзор покрытия неизвестен: API недоступен ({state.detail}). Это не означает отсутствие данных.</Alert>;
  const all = state.data.instruments;
  const withData = all.filter((row) => row.totalCandles > 0).length;
  const needle = filter.trim().toLowerCase();
  const rows = all.filter((row) => (!withDataOnly || row.totalCandles > 0) && row.symbol.toLowerCase().includes(needle));
  return <section aria-labelledby="coverage-overview-heading" className="grid gap-3">
    <SectionHeader id="coverage-overview-heading" title="Обзор покрытия истории" />
    <p className="text-xs text-muted-foreground">Инструментов со свечами: {withData} из {all.length}. «Пропущено минут» — суммарные разрывы между первой и последней сохранённой минутой инструмента; точные интервалы — в детальном покрытии ниже.</p>
    <div className="flex flex-wrap items-end gap-4">
      <Field label="Фильтр по символу" className="min-w-48">{(control) => <Input value={filter} onChange={(event) => setFilter(event.target.value)} {...control} />}</Field>
      <label className="flex items-center gap-2 pb-1 text-sm"><Checkbox checked={withDataOnly} onCheckedChange={(checked) => setWithDataOnly(checked === true)} /> Только инструменты со свечами</label>
    </div>
    <div className="max-h-96 overflow-auto rounded-md border border-input"><Table>
      <TableHeader className="sticky top-0 bg-background"><TableRow><TableHead>Инструмент</TableHead><TableHead>Статус</TableHead><TableHead>Первая минута</TableHead><TableHead>Последняя минута</TableHead><TableHead numeric>Свечей</TableHead><TableHead numeric>Пропущено минут</TableHead><TableHead numeric>Из них без торговли</TableHead></TableRow></TableHeader>
      <TableBody>{rows.slice(0, 200).map((row) => <TableRow key={row.instrumentId}>
        <TableCell className="font-medium">{row.symbol}</TableCell>
        <TableCell>{row.status}</TableCell>
        <TableCell>{row.firstMinuteUnix === null ? "нет" : localTime(row.firstMinuteUnix)}</TableCell>
        <TableCell>{row.lastMinuteUnix === null ? "нет" : localTime(row.lastMinuteUnix)}</TableCell>
        <TableCell numeric>{row.totalCandles}</TableCell>
        <TableCell numeric>{row.totalCandles === 0 ? "—" : row.missingMinutes}</TableCell>
        <TableCell numeric>{row.totalCandles === 0 ? "—" : row.verifiedMinutes}</TableCell>
      </TableRow>)}</TableBody>
    </Table></div>
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
  const plans = usePlatformPoll<{ plans: BackfillPlanRow[] }>("/v1/history/backfill-plans", revision);
  const repairs = usePlatformPoll<{ jobs: GapRepairRow[] }>("/v1/history/gap-repairs", revision);
  const [instrumentId, setInstrumentId] = useState("");
  const [collectionSymbols, setCollectionSymbols] = useState("");
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
  const counts = jobs.kind === "ready" && jobs.data.counts ? jobs.data.counts : null;
  const activeJobs = counts === null ? 0 : counts.queued + counts.running + counts.cancel_requested;
  const activePlans = plans.kind === "ready" ? plans.data.plans.filter((plan) => plan.status === "active") : [];
  const pendingPlanItems = activePlans.reduce((sum, plan) => sum + plan.counts.pending, 0);
  const instrumentName = (id: number) => instruments.find((instrument) => instrument.id === id)?.symbol ?? `ID ${id}`;

  const collectionSelection = collectionSymbols.split(/[\s,;]+/).filter(Boolean);
  const collectionSelectionIds = instruments.filter((instrument) => collectionSelection.includes(instrument.symbol)).map((instrument) => instrument.id);

  async function importListingToCollection() {
    setBusy(true); setError(null);
    try {
      const response = await apiFetch("/v1/history/instruments/listing");
      const payload = await response.json() as { instruments?: { id: number; symbol: string }[]; error?: string };
      if (!response.ok || !payload.instruments) throw new Error(payload.error ?? `HTTP ${response.status}`);
      const merged = new Set(collectionSelection);
      for (const instrument of payload.instruments) merged.add(instrument.symbol);
      setCollectionSymbols([...merged].join("\n"));
      setRevision((value) => value + 1);
      setMessage(`Листинг Bybit загружен: ${payload.instruments.length} инструментов. Выбрано: ${merged.size}.`);
    } catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }

  async function bulkSetCollection(enabled: boolean) {
    setBusy(true); setError(null); setMessage(null);
    try {
      if (collectionSelectionIds.length === 0) throw new Error("Выберите хотя бы один инструмент.");
      let updated = 0;
      for (let index = 0; index < collectionSelectionIds.length; index += 1000) {
        const chunk = collectionSelectionIds.slice(index, index + 1000);
        const response = await apiFetch("/v1/history/collection-targets", {
          method: "PUT",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ instrumentIds: chunk, enabled }),
        });
        const payload = await response.json() as { updated?: number; error?: string };
        if (!response.ok) throw new Error(payload.error ?? `HTTP ${response.status}`);
        updated += payload.updated ?? 0;
      }
      setRevision((value) => value + 1);
      setMessage(enabled
        ? `Сбор включён для ${updated} инструментов. Инструменты с историей сначала дозаполнят хвост от последней свечи до текущей закрытой минуты (задания видны во вкладке «Задания загрузки»), затем перейдут к поминутному сбору. Инструменты без сохранённой истории ждут явной догрузки.`
        : `Сбор выключен для ${updated} инструментов.`);
    } catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }

  async function startGapRepairs() {
    setBusy(true); setError(null); setMessage(null);
    try {
      if (collectionSelectionIds.length === 0) throw new Error("Выберите хотя бы один инструмент.");
      let created = 0;
      for (let index = 0; index < collectionSelectionIds.length; index += 1000) {
        const chunk = collectionSelectionIds.slice(index, index + 1000);
        const response = await apiFetch("/v1/history/gap-repairs", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ instrumentIds: chunk }),
        });
        const payload = await response.json() as { created?: number; error?: string };
        if (!response.ok) throw new Error(payload.error ?? `HTTP ${response.status}`);
        created += payload.created ?? 0;
      }
      setRevision((value) => value + 1);
      setMessage(`Создано операций ремонта пропусков: ${created}. Заполняются только реальные пропуски между сохранёнными свечами; минуты без торговли подтверждаются маркерами.`);
    } catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }

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
      const response = await apiFetch("/v1/history/backfill-plans", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ instrumentIds: pickedInstruments.map((instrument) => instrument.id), ...range }),
      });
      const payload = await response.json() as { plan?: BackfillPlanRow; error?: string };
      if (!response.ok || !payload.plan) throw new Error(payload.error ?? `HTTP ${response.status}`);
      const plan = payload.plan;
      setRevision((value) => value + 1);
      setMessage(`Создан план загрузки #${plan.id}: ${plan.counts.total} инструментов. Сервер сам ставит задания в очередь по мере освобождения лимитов — сейчас поставлено ${plan.counts.enqueued}, ожидает ${plan.counts.pending}${plan.counts.failed > 0 ? `, с невыполнимым объёмом: ${plan.counts.failed}` : ""}.`);
    } catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }

  return <section aria-labelledby="history-heading" className="grid gap-5">
    <PageHeader id="history-heading" title="История и сбор" description={<>Загрузка истории и текущий сбор независимы от replay и live-сканера. Действия выполняются только по вашей команде. Время: {localTimezoneLabel()}.</>} />
    {jobs.kind === "unreachable" && <Alert variant="destructive" role="alert">Статус массовых загрузок неизвестен ({jobs.detail}). Это не означает, что загрузок нет.</Alert>}
    {plans.kind === "unreachable" && <Alert variant="destructive" role="alert">Планы загрузки неизвестны ({plans.detail}). Это не означает, что планов нет.</Alert>}
    {activePlans.length > 0 && <p role="status" className="text-sm text-muted-foreground">Активных планов загрузки: {activePlans.length}. Инструментов ожидают освобождения лимитов очереди: {pendingPlanItems}.</p>}
    {counts !== null && (activeJobs > 0
      ? <Alert variant="info" role="status">Идёт массовая загрузка истории: активных заданий {activeJobs} (в очереди {counts.queued}, выполняется {counts.running}, запрошена отмена {counts.cancel_requested}). Статус хранится на сервере и виден после перезагрузки страницы.</Alert>
      : <p role="status" className="text-sm text-muted-foreground">Массовых загрузок, запущенных пользователем, сейчас нет. Постоянный ежеминутный сбор закрытых минут работает отдельно и здесь не учитывается.</p>)}
    <SegmentedList role="tablist" aria-label="Разделы «История и сбор»">
      {HISTORY_TABS.map((item) => <SegmentedTrigger key={item.id} role="tab" aria-selected={tab === item.id} onClick={() => setTab(item.id)}>{item.label}</SegmentedTrigger>)}
    </SegmentedList>
    {error && <Alert variant="destructive" role="alert">{error}</Alert>}
    {message && <Alert variant="success" role="status">{message}</Alert>}

    {tab === "upload" && <section className="grid gap-3" aria-labelledby="upload-heading">
      <SectionHeader id="upload-heading" title="Загрузить историю" />
      <p className="text-xs text-muted-foreground">Выберите инструменты вручную, импортом CSV без заголовка или текущим листингом Bybit, при необходимости отредактируйте список и подтвердите загрузку для выбранных дат. Создаётся отдельное задание на каждый инструмент.</p>
      <div className="flex flex-wrap gap-2">
        <Button type="button" variant="outline" disabled={busy} onClick={() => { void importListing(); }}>Импортировать текущий листинг Bybit (линейные USDT perpetual)</Button>
        <Button type="button" variant="outline" disabled={busy || symbols.trim().length === 0} onClick={() => { setSymbols(""); setMessage(null); }}>Очистить выбор</Button>
      </div>
      <InstrumentPicker catalog={instruments} symbols={symbols} onChange={(value) => { setSymbols(value); setMessage(null); }} disabled={busy || catalog.kind !== "ready"} />
      <Card asChild><form className="grid max-w-2xl gap-3" onSubmit={(event) => { void enqueue(event); }}>
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Начало загрузки">{(control) => <Input type="datetime-local" required value={from} onChange={(event) => setFrom(utcMidnightOnDateChange(from, event.target.value))} {...control} />}</Field>
          <Field label="Конец загрузки">{(control) => <Input type="datetime-local" required value={to} onChange={(event) => setTo(utcMidnightOnDateChange(to, event.target.value))} {...control} />}</Field>
        </div>
        <p className="text-sm text-muted-foreground">Выбрано инструментов: {pickedInstruments.length}.</p>
        <div><Button type="submit" disabled={busy || pickedInstruments.length === 0}>Создать план загрузки ({pickedInstruments.length})</Button></div>
      </form></Card>
      {plans.kind === "ready" && plans.data.plans.length > 0 && <div className="overflow-x-auto"><Table>
        <caption className="text-left text-muted-foreground">Планы загрузки</caption>
        <TableHeader><TableRow><TableHead>План</TableHead><TableHead>Диапазон</TableHead><TableHead>Статус</TableHead><TableHead>Поставлено / ждёт / ошибки</TableHead><TableHead numeric>Активных заданий</TableHead><TableHead>Действие</TableHead></TableRow></TableHeader>
        <TableBody>{plans.data.plans.map((plan) => <TableRow key={plan.id}>
          <TableCell className="font-medium">#{plan.id}</TableCell>
          <TableCell>{localTime(plan.fromUnix)} – {localTime(plan.toUnix)}</TableCell>
          <TableCell><Badge variant={plan.status === "active" ? "info" : plan.status === "completed" ? "success" : "neutral"}>{plan.status === "active" ? "активен" : plan.status === "completed" ? "все поставлены в очередь" : "отменён"}</Badge></TableCell>
          <TableCell numeric>{plan.counts.enqueued} / {plan.counts.pending} / {plan.counts.failed}</TableCell>
          <TableCell numeric>{plan.activeJobs}</TableCell>
          <TableCell>{plan.status === "active" && <Button variant="outline" size="sm" disabled={busy} onClick={() => { void mutation(`/v1/history/backfill-plans/${plan.id}/cancel`, {}, "POST", `Отмена плана #${plan.id} запрошена: новые задания больше не ставятся, активные получают запрос отмены.`); }}>Отменить план {plan.id}</Button>}</TableCell>
        </TableRow>)}</TableBody>
      </Table></div>}
    </section>}

    {tab === "collection" && <section className="grid gap-3" aria-labelledby="collection-heading">
      <SectionHeader id="collection-heading" title="Текущий сбор свечей" />
      <CoverageOverview revision={revision} />
      <Card className="grid max-w-2xl gap-3">
        <p className="text-sm text-muted-foreground">Массовое управление сбором: выберите инструменты чек-боксами, «Все из БД» выбирает весь каталог, импорт листинга Bybit обновляет каталог биржи и добавляет её символы в выбор. Лимита на количество нет.</p>
        <div><Button type="button" variant="outline" disabled={busy} onClick={() => { void importListingToCollection(); }}>Импортировать листинг для сбора</Button></div>
        <InstrumentPicker catalog={instruments} symbols={collectionSymbols} onChange={(value) => { setCollectionSymbols(value); setMessage(null); }} disabled={busy || catalog.kind !== "ready"} limit={null} />
        <div className="flex flex-wrap gap-2">
          <Button type="button" disabled={busy || collectionSelectionIds.length === 0} onClick={() => { void bulkSetCollection(true); }}>Включить сбор для выбранных ({collectionSelectionIds.length})</Button>
          <Button type="button" variant="outline" disabled={busy || collectionSelectionIds.length === 0} onClick={() => { void bulkSetCollection(false); }}>Выключить сбор для выбранных</Button>
          <Button type="button" variant="outline" disabled={busy || collectionSelectionIds.length === 0} onClick={() => { void startGapRepairs(); }}>Проверить и заполнить пропуски ({collectionSelectionIds.length})</Button>
        </div>
      </Card>
      <div className="flex flex-wrap items-end gap-3">
        <Field label="Инструмент БД" className="min-w-56 flex-1 sm:max-w-md">{(control) => <Select value={instrumentId} disabled={catalog.kind !== "ready" || busy} onChange={(event) => setInstrumentId(event.target.value)} {...control}><option value="">Выберите инструмент</option>{instruments.map((instrument) => <option key={instrument.id} value={instrument.id}>{instrument.exchange} / {instrument.category} / {instrument.symbol} ({instrument.status})</option>)}</Select>}</Field>
        <Button variant="outline" disabled={busy} onClick={() => setRevision((value) => value + 1)}>Обновить историю</Button>
      </div>
      {catalog.kind === "loading" && <p className="text-sm text-muted-foreground">Загрузка каталога…</p>}{catalog.kind === "unreachable" && <Alert variant="destructive" role="alert">Каталог недоступен ({catalog.detail})</Alert>}
      {selected && <InstrumentCoverage key={selected.id} id={selected.id} revision={revision} />}
      {targets.kind === "loading" && <p className="text-sm text-muted-foreground">Загрузка состояния сбора…</p>}{targets.kind === "unreachable" && <Alert variant="destructive" role="alert">Состояние сбора неизвестно ({targets.detail}). Управление недоступно.</Alert>}
      {targets.kind === "ready" && <><p>{selected ? `Выбранный инструмент: ${target?.enabled ? "сбор включён" : "сбор выключен"}.` : "Выберите инструмент для управления сбором."}</p><div><Button variant="outline" disabled={busy || !selected} onClick={() => { if (selected) void mutation(`/v1/history/collection-targets/${selected.id}`, { enabled: !target?.enabled }, "PUT", "Настройка сбора сохранена."); }}>{target?.enabled ? "Отключить сбор" : "Включить сбор"}</Button></div>
        <div className="overflow-x-auto"><Table>
          <caption className="text-left text-muted-foreground">Первые 100 целей сбора из ответа API</caption>
          <TableHeader><TableRow><TableHead>Инструмент</TableHead><TableHead>Включён</TableHead><TableHead>Последняя собранная минута</TableHead></TableRow></TableHeader>
          <TableBody>{targets.data.targets.slice(0, 100).map((item) => <TableRow key={item.instrumentId}>
            <TableCell className="font-medium">{instrumentName(item.instrumentId)}</TableCell>
            <TableCell>{item.enabled ? "да" : "нет"}</TableCell>
            <TableCell>{item.lastCollectedUnix === null ? "нет данных" : localTime(item.lastCollectedUnix)}</TableCell>
          </TableRow>)}</TableBody>
        </Table>{targets.data.targets.length === 0 && <p>Цели сбора ещё не настроены.</p>}</div>
        {repairs.kind === "ready" && repairs.data.jobs.length > 0 && <div className="overflow-x-auto"><Table>
          <caption className="text-left text-muted-foreground">Операции ремонта пропусков (последние 100)</caption>
          <TableHeader><TableRow><TableHead>ID / инструмент</TableHead><TableHead>Статус</TableHead><TableHead numeric>Пропусков обработано</TableHead><TableHead numeric>Заполнено минут</TableHead><TableHead numeric>Подтверждено без торговли</TableHead><TableHead>Ошибка</TableHead></TableRow></TableHeader>
          <TableBody>{repairs.data.jobs.slice(0, 100).map((job) => <TableRow key={job.id}>
            <TableCell className="font-medium">#{job.id} · {job.symbol}</TableCell>
            <TableCell><Badge variant={job.status === "done" ? "success" : job.status === "failed" ? "danger" : job.status === "running" ? "info" : "neutral"}>{job.status === "queued" ? "в очереди" : job.status === "running" ? "выполняется" : job.status === "done" ? "завершено" : "ошибка"}</Badge></TableCell>
            <TableCell numeric>{job.gapsProcessed}</TableCell>
            <TableCell numeric>{job.filledMinutes}</TableCell>
            <TableCell numeric>{job.verifiedMinutes}</TableCell>
            <TableCell>{job.error && <span role="alert" className="block max-w-sm break-words text-danger">{job.error}</span>}</TableCell>
          </TableRow>)}</TableBody>
        </Table></div>}</>}
    </section>}

    {tab === "jobs" && <section className="grid gap-3" aria-labelledby="backfill-heading">
      <SectionHeader id="backfill-heading" title="Задания загрузки" /><p className="text-xs text-muted-foreground">Обновление каждые 3 секунды в видимой вкладке. Прогресс отражает checkpoint диапазона, не полноту свечей.</p>
      {counts !== null && <p className="text-sm text-muted-foreground">Активных: {activeJobs}. Завершено успешно: {counts.done}. С ошибкой: {counts.failed}. Отменено: {counts.cancelled}.</p>}
      {jobs.kind === "loading" && <p className="text-sm text-muted-foreground">Загрузка заданий…</p>}{jobs.kind === "unreachable" && <Alert variant="destructive" role="alert">Задания недоступны ({jobs.detail})</Alert>}
      {jobs.kind === "ready" && <div className="overflow-x-auto"><Table>
        <caption className="text-left text-muted-foreground">Первые 100 заданий загрузки из ответа API</caption>
        <TableHeader><TableRow><TableHead>ID / инструмент</TableHead><TableHead>Диапазон</TableHead><TableHead>Статус</TableHead><TableHead>Checkpoint</TableHead><TableHead>Действие</TableHead></TableRow></TableHeader>
        <TableBody>{jobs.data.jobs.slice(0, 100).map((job) => <TableRow key={job.id}>
          <TableCell className="font-medium">#{job.id} · {instrumentName(job.instrumentId)}</TableCell>
          <TableCell>{localTime(job.fromUnix)} – {localTime(job.toUnix)}</TableCell>
          <TableCell><Badge variant={job.status === "done" ? "success" : job.status === "failed" ? "danger" : job.status === "running" ? "info" : job.status === "cancel_requested" ? "warning" : "neutral"}>{job.status}</Badge>{job.error && <p className="mt-1 max-w-sm break-words text-xs text-danger" role="alert">{job.error}</p>}</TableCell>
          <TableCell>{job.checkpointUnix === null ? "ещё нет" : `${localTime(job.checkpointUnix)} (${Math.round(Math.max(0, Math.min(1, (job.checkpointUnix - job.fromUnix) / Math.max(1, job.toUnix - job.fromUnix))) * 100)}%)`}</TableCell>
          <TableCell>{["queued", "running", "cancel_requested"].includes(job.status) && <Button variant="outline" size="sm" disabled={busy || job.status === "cancel_requested"} onClick={() => { void mutation(`/v1/history/backfill-jobs/${encodeURIComponent(String(job.id))}/cancel`, {}, "POST", `Отмена задания #${job.id} запрошена.`); }}>Отменить загрузку {job.id}</Button>}</TableCell>
        </TableRow>)}</TableBody>
      </Table>{jobs.data.jobs.length === 0 && <p>Заданий загрузки пока нет.</p>}</div>}
    </section>}
  </section>;
}
