import { useEffect, useRef, useState, type FormEvent } from "react";
import { Alert } from "../components/ui/alert";
import { Badge } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import { Checkbox } from "../components/ui/checkbox";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "../components/ui/dialog";
import { Field } from "../components/ui/field";
import { Input } from "../components/ui/input";
import { Spinner } from "../components/ui/spinner";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "../components/ui/table";
import { effectiveTimezone, formatIsoDateTime, formatUnix } from "../lib/datetime-format";
import { fetchPlatform } from "../lib/platform-api";
import { MAX_CONTINUOUS_LOOKBACK_DAYS, MIN_CONTINUOUS_LOOKBACK_DAYS, type ContinuousSlot } from "../lib/continuous-api";
import { getOptimizationDraft } from "../lib/optimization-api";
import { ANALYTICS_BASELINE, ANALYTICS_SEARCH_FIELDS, buildContinuousConfig, initialDomains, initialScope, restoreDomains, type ContinuousDraft, type ScopeDraft } from "./optimization-form";
import { OptimizationFields } from "./optimization-fields";
import type { CatalogInstrument } from "./instrument-picker";
import { algorithmDisplayName, errorText, type Algorithm } from "./platform-controls";
import type { ContinuousController } from "./use-continuous-study";
import { applyOptimizationDraftScope, createOptimizationDraft, restoreOptimizationDraft, useOptimizationDraftSave, type OptimizationFormDraft } from "./optimization-draft";

const statuses = { stopped: "Остановлено", running: "Непрерывное исследование включено", stopping: "Останавливается", error: "Ошибка непрерывного исследования" };
const phases: Record<string, string> = { queued: "Ожидание очереди", idle: "Ожидание следующего исследования", research: "Исследование", acquiring: "Подготовка снимка", replay: "Служебный replay", analytics: "Аналитика сохранённого набора", cleanup: "Очистка незавершённых данных" };
const slotStatuses: Record<string, string> = { idle: "Ожидает исследования", active: "Исследование принято", warming: "Исследование принято", inactive: "Профиль неактивен", pending: "Ожидает исследования", running: "Подготовка поколения", researching: "Исследование", error: "Техническая ошибка", failed: "Техническая ошибка", nonpass: "Допуск не пройден", rejected: "Допуск не пройден", no_winner: "Нет оценимого победителя", stopped: "Остановлен" };
const pendingStatuses: Record<string, string> = { warming: "Инициализация live по текущим данным", ready: "Готов к переключению · ещё не действует", error: "Ошибка инициализации live" };
const closedTime = (time: number) => `${formatUnix(time)} (${effectiveTimezone()})`;
const waitingReasonLabels: Record<string, string> = {
  insufficient_history: "Недостаточно истории", history_missing: "Недостаточно истории", waiting_tail: "Ожидание свежих закрытых минут",
  waiting_data: "Ожидание данных", recent_gap: "Пропуски последних минут", gap_tolerance: "Превышен допуск пропусков",
  missing_price_reference: "Нет опорной цены", waiting_resource: "Ожидание ресурса", instrument_unavailable: "Инструмент недоступен",
  readiness_unknown: "Готовность неизвестна", unknown: "Готовность неизвестна",
};
function waitingReasonLabel(reason: string): string {
  if (Object.hasOwn(waitingReasonLabels, reason)) return waitingReasonLabels[reason]!;
  return `Другая причина: ${reason.slice(0, 120).replace(/\s+/g, " ")}${reason.length > 120 ? "…" : ""}`;
}

function SlotReadiness({ slot }: { slot: ContinuousSlot }) {
  const summary = slot.liveReadinessSummary;
  if (!summary || slot.activeGenerationId === null || slot.activeStatus === "inactive" || summary.generationId !== slot.activeGenerationId) return <span>Готовность инструментов неизвестна.</span>;
  const shown = [...summary.waitingReasons].filter((entry) => entry.count > 0).sort((a, b) => b.count - a.count || a.reason.localeCompare(b.reason)).slice(0, 3);
  const other = summary.waitingInstruments - shown.reduce((total, entry) => total + entry.count, 0);
  return <div className="grid gap-1">
    <span>Готовы {summary.readyInstruments} из {summary.totalInstruments} · ожидают {summary.waitingInstruments}</span>
    {summary.waitingInstruments > 0 && <>
      <ul aria-label="Причины ожидания инструментов" className="grid gap-1">
        {shown.map((entry, index) => <li key={index} className="break-words">{waitingReasonLabel(entry.reason)}: {entry.count}</li>)}
        {other > 0 && <li>Другие причины ожидания: {other}</li>}
      </ul>
      <span>Ожидающие инструменты остаются в наборе и подключаются к тому же поколению после инициализации.</span>
    </>}
    <span className="text-muted-foreground">Это готовность инициализации, не подтверждение формирования или доставки сигналов.</span>
  </div>;
}

function SlotIncumbent({ slot }: { slot: ContinuousSlot }) {
  if (slot.activeGenerationId === null) return <div className="grid gap-1 text-xs"><span className="text-sm">Нет действующего поколения</span><span>Адаптивные сигналы этого профиля не формируются.</span><SlotReadiness slot={slot} /></div>;
  return <div className="grid gap-1 text-xs">
    <span className="text-sm">{slot.activeStatus === "active" ? "Действующее" : "Принятое"} поколение #{slot.activeGenerationId}</span>
    <Badge>{slot.activeStatus === "active" ? "Активно" : slot.activeStatus === "inactive" ? "Неактивно" : "Состояние live неизвестно (старый API)"}</Badge>
    <span>Принятое поколение сохранено</span>
    <SlotReadiness slot={slot} />
    <span>{slot.liveWatermark == null ? "Watermark переключения неизвестен" : `Watermark переключения: ${closedTime(slot.liveWatermark)}`}</span>
  </div>;
}

function pendingErrorMessage(value: unknown): string | null {
  if (value === undefined || value === null) return null;
  if (typeof value === "string" && value.trim()) return value.slice(0, 300);
  if (typeof value === "object" && !Array.isArray(value)) {
    const detail = value as { code?: unknown; message?: unknown };
    const parts = [typeof detail.code === "string" ? detail.code.slice(0, 80) : "", typeof detail.message === "string" ? detail.message.slice(0, 300) : ""].filter(Boolean);
    if (parts.length) return parts.join(": ");
  }
  // serde_json::Value can be a scalar/array too; never dump opaque payload or invent readiness.
  return "Сервер не передал текстовое описание ошибки.";
}

function SlotPending({ slot }: { slot: ContinuousSlot }) {
  const legacy = slot.pendingGenerationId === undefined && slot.pendingStatus === undefined;
  const absent = slot.pendingGenerationId === null && slot.pendingStatus === null;
  const error = pendingErrorMessage(slot.pendingError) ?? (slot.pendingStatus === "error" ? "Описание ошибки не передано." : null);
  return <div className="grid gap-1 text-xs">
    {slot.pendingGenerationId != null && <span className="text-sm">Кандидат #{slot.pendingGenerationId}</span>}
    {absent ? <span>Нет кандидата на замену</span> : <Badge variant={slot.pendingStatus === "error" ? "danger" : "neutral"}>{legacy ? "Статус инициализации live неизвестен (старый API)" : slot.pendingStatus ? pendingStatuses[slot.pendingStatus] ?? `Состояние кандидата: ${slot.pendingStatus.slice(0, 120)}` : "Статус инициализации live неизвестен"}</Badge>}
    {!legacy && !absent && <span>{slot.readinessFrontier === undefined ? "Граница готовности неизвестна" : slot.readinessFrontier === null ? "Граница готовности ещё не подтверждена" : `Граница готовности (закрытие минуты): ${closedTime(slot.readinessFrontier)}`}</span>}
    {error && <p role="alert" className="text-destructive">Ошибка кандидата: {error}</p>}
  </div>;
}

function hasNoProfileResults(slot: ContinuousSlot): boolean {
  return slot.studyId === null && slot.activeGenerationId === null && slot.pendingGenerationId == null
    && slot.meanExpectancy === null && slot.selectedCell === null && slot.pendingStatus == null
    && slot.pendingError == null && slot.readinessFrontier == null && slot.liveWatermark == null;
}


export function AdaptiveLive({ controller }: { controller: ContinuousController }) {
  const savedPermission = controller.process?.config?.adaptiveEnabled ?? false;
  const [draft, setDraft] = useState<boolean | null>(null);
  const [saved, setSaved] = useState(false);
  const enabled = draft ?? savedPermission;
  const accepted = controller.process?.slots.filter((slot) => slot.activeGenerationId !== null).length ?? 0;
  return <div className="grid gap-3">
    <label className="flex items-center gap-2 text-sm"><Checkbox checked={enabled} disabled={!controller.available || controller.busy !== null} onCheckedChange={(checked) => { setDraft(checked === true); setSaved(false); }} />Разрешить адаптивное live-сканирование</label>
    <p className="text-sm text-muted-foreground">Отдельное разрешение; исследование не включает live автоматически. Постоянные параметры не меняются. Готовность данных и общий выключатель live проверяются отдельно.</p>
    <p role="status" className="text-sm">{!controller.available ? "Состояние адаптивного live не подтверждено." : !savedPermission ? "Адаптивный live не разрешён." : accepted === 0 ? "Адаптивный live разрешён · действующих профилей нет, ожидается инициализация." : `Адаптивный live разрешён · принято профилей: ${accepted}.`}</p>
    <div><Button type="button" variant="outline" disabled={!controller.available || !controller.process?.config || controller.busy !== null || enabled === savedPermission} onClick={() => {
      const config = controller.process?.config;
      if (config) void controller.save({ ...config, adaptiveEnabled: enabled }).then((ok) => { if (ok) { setDraft(null); setSaved(true); } });
    }}>Сохранить разрешение адаптивного live</Button></div>
    {controller.available && !controller.process?.config && <p className="text-xs text-muted-foreground">Сначала сохраните настройки непрерывного исследования в разделе «Оптимизации».</p>}
    {saved && <p role="status" className="text-sm">Разрешение адаптивного live сохранено.</p>}
  </div>;
}

function ContinuousForm({ algorithm, controller, onSaved }: { algorithm: Algorithm; controller: ContinuousController; onSaved: () => void }) {
  const saved = controller.process?.config;
  const baseline = saved ? { ...algorithm, params: saved.study.algorithmParams } : algorithm;
  const [scope, setScope] = useState<ScopeDraft>(() => saved ? { ...initialScope(), symbols: saved.study.instruments.map((item) => item.symbol).join("\n"), direction: saved.study.direction,
    horizonMinutes: String(saved.study.horizonMinutes), tpLevels: saved.study.tpLevels.join(", "), slLevels: saved.study.slLevels.join(", "), minClosedPerCell: String(saved.study.minClosedPerCell), seed: String(saved.study.seed), maxTrials: String(saved.study.maxTrials), maxSeconds: String(saved.study.maxSeconds) } : initialScope());
  const [draft, setDraft] = useState<ContinuousDraft>(() => ({ lookbackDays: String(saved?.lookbackDays ?? 30), minSetupsPerDay: (saved?.minSetupsPerDay ?? [4]).join(", "), minMeanExpectancy: String(saved?.minMeanExpectancy ?? 0.15), adaptiveEnabled: saved?.adaptiveEnabled ?? false }));
  const [algorithmDomains, setAlgorithmDomains] = useState(() => saved ? restoreDomains(algorithm.fields, saved.study.algorithmParams, saved.study.algorithmSearch) : initialDomains(algorithm.fields, algorithm.params));
  const [analyticsDomains, setAnalyticsDomains] = useState(() => saved ? restoreDomains(ANALYTICS_SEARCH_FIELDS, { ...saved.study.analyticsParams }, saved.study.analyticsSearch) : initialDomains(ANALYTICS_SEARCH_FIELDS, { ...ANALYTICS_BASELINE }));
  const [catalog, setCatalog] = useState<CatalogInstrument[]>([]);
  const [ready, setReady] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [retry, setRetry] = useState(0);
  const locked = controller.process !== null && controller.process.status !== "stopped";
  const storedDraft = useRef<OptimizationFormDraft | null>(null);
  const initialValues = useRef({ scope, algorithmDomains, analyticsDomains });
  const draftSave = useOptimizationDraftSave(algorithm.id);
  useEffect(() => {
    const request = new AbortController();
    void Promise.all([fetchPlatform<{ instruments: CatalogInstrument[] }>("/v1/instruments", request.signal), fetchPlatform<{ value: { replay?: { symbols?: string } } }>("/v1/settings/ui", request.signal), getOptimizationDraft(algorithm.id, request.signal)]).then(([instruments, preferences, rawDraft]) => {
      if (request.signal.aborted) return;
      if (instruments.kind !== "ready" || preferences.kind !== "ready") { setError("Historical-каталог или сохранённый выбор недоступны. Сохранение заблокировано; повторите загрузку."); return; }
      const items = instruments.data.instruments.filter((item) => item.exchange === "bybit" && item.category === "linear");
      const initial = initialValues.current;
      const fallbackScope = initial.scope.symbols ? initial.scope : { ...initial.scope, symbols: preferences.data.value.replay?.symbols ?? items.map((item) => item.symbol).join("\n") };
      const restored = restoreOptimizationDraft(rawDraft, algorithm.fields, ANALYTICS_SEARCH_FIELDS, fallbackScope, initial.algorithmDomains, initial.analyticsDomains);
      storedDraft.current = restored;
      setCatalog(items);
      if (restored && !locked) {
        setScope(applyOptimizationDraftScope(fallbackScope, restored, "continuous"));
        setDraft((previous) => ({ ...previous, ...restored.continuous }));
        setAlgorithmDomains(restored.shared.algorithmDomains);
        setAnalyticsDomains(restored.shared.analyticsDomains);
      } else setScope(fallbackScope);
      setReady(true); setError(null);
    }).catch(() => {
      if (!request.signal.aborted) setError("Не удалось загрузить сохранённые настройки оптимизации. Сохранение заблокировано; повторите загрузку.");
    });
    return () => request.abort();
  }, [algorithm, locked, retry]);
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    // Radix portals retain React ancestry: never bubble this submit into the fixed-settings form.
    event.stopPropagation();
    if (!ready || locked || controller.busy !== null || !controller.available) return;
    setError(null);
    try {
      const config = buildContinuousConfig(baseline, scope, catalog, algorithmDomains, analyticsDomains, { ...draft, adaptiveEnabled: controller.process?.config?.adaptiveEnabled ?? draft.adaptiveEnabled });
      if (await controller.save(config)) onSaved();
    } catch (cause) { setError(errorText(cause)); }
  }
  function saveDraft(): void {
    if (!ready || locked || controller.busy !== null) return;
    const next = createOptimizationDraft(scope, algorithmDomains, analyticsDomains, storedDraft.current, {
      continuous: { lookbackDays: draft.lookbackDays, minSetupsPerDay: draft.minSetupsPerDay, minMeanExpectancy: draft.minMeanExpectancy },
    });
    storedDraft.current = next;
    draftSave.save(next);
  }
  return <form noValidate onSubmit={(event) => { void submit(event); }} onBlur={saveDraft} className="grid min-w-0 gap-5">
    <Alert>Сохранение не запускает исследование и не включает live. Сервер фиксирует последние N суток при фактическом старте, без lag; вся цепочка использует один период и снимок. Включённая кластеризация выбирает только первый сетап.</Alert>
    {locked && <Alert>Параметры исследования доступны для чтения. Чтобы изменить период, пороги или домены, остановите процесс и дождитесь drain. Разрешение adaptive live сохраняется отдельно.</Alert>}
    <fieldset disabled={locked || controller.busy !== null} className="grid gap-3 sm:grid-cols-2">
      <legend className="mb-2 font-semibold">Период и отдельные пороговые профили</legend>
      <Field label="Последние N дней" hint="От 1 минуты до 3660 суток, в целых секундах; дробные сутки разрешены. Окно фиксируется сервером при старте.">{(control) => <Input {...control} type="number" min={MIN_CONTINUOUS_LOOKBACK_DAYS} max={MAX_CONTINUOUS_LOOKBACK_DAYS} step="any" value={draft.lookbackDays} onChange={(event) => setDraft({ ...draft, lookbackDays: event.target.value })} />}</Field>
      <Field label="Минимум сетапов в день — список профилей" hint="До 64 уникальных значений через запятую; 0 отключает частотный допуск. Не поисковый домен и не обещание числа live-сообщений.">{(control) => <Input {...control} maxLength={8192} value={draft.minSetupsPerDay} onChange={(event) => setDraft({ ...draft, minSetupsPerDay: event.target.value })} />}</Field>
      <Field label="Минимальное среднее матожидание, R" hint="Гейт всей неокруглённой матрицы, а не лучшей ячейки. По умолчанию 0.15 R.">{(control) => <Input {...control} type="number" step="any" value={draft.minMeanExpectancy} onChange={(event) => setDraft({ ...draft, minMeanExpectancy: event.target.value })} />}</Field>
    </fieldset>
    {!ready && !error && <Spinner label="Загрузка historical-списка" />}
    {(error ?? draftSave.saveError) && <Alert variant="destructive" role="alert">{error ?? draftSave.saveError}{!ready && <Button type="button" variant="outline" onClick={() => setRetry((value) => value + 1)}>Повторить загрузку</Button>}</Alert>}
    <OptimizationFields algorithm={baseline} scope={scope} onScopeChange={(patch) => { setScope((previous) => ({ ...previous, ...patch })); setError(null); }} catalog={catalog} ready={ready} disabled={locked || controller.busy !== null} algorithmDomains={algorithmDomains} onAlgorithmDomainsChange={setAlgorithmDomains} analyticsDomains={analyticsDomains} onAnalyticsDomainsChange={setAnalyticsDomains} continuous />
    {controller.error && <Alert variant="destructive" role="alert">{controller.error}</Alert>}
    <div><Button type="submit" disabled={!ready || locked || !controller.available || controller.busy !== null}>{controller.busy === "save" ? "Сохранение…" : "Сохранить непрерывное исследование"}</Button></div>
  </form>;
}

export function ContinuousResearch({ algorithm, controller }: { algorithm: Algorithm; controller: ContinuousController }) {
  const [open, setOpen] = useState(false);
  const process = controller.process;
  const recoveryExhausted = process?.status === "error" && process.error?.code === "durable_resume_retry_exhausted"
    && process.config !== null && process.activeStudyId !== null;
  return <section aria-label="Непрерывные исследования" className="grid min-w-0 gap-3">
    <h5 className="font-semibold">Настроенные адаптивные профили</h5>
    {controller.loading && <Spinner label="Проверка непрерывного процесса" />}
    {controller.error && <Alert variant="destructive" role="alert">{controller.error}</Alert>}
    {!controller.available && process && <p className="text-sm">Показан последний подтверждённый снимок; актуальность не подтверждена.</p>}
    {controller.available && <p role="status" className="text-sm">{process ? statuses[process.status] : "Процесс не настроен. Исследование не запущено."}</p>}
    {process && <>
      <p className="text-sm">Ревизия конфигурации: {process.configRevision}{process.activeStudyId !== null ? ` · Исследование #${process.activeStudyId}` : ""}{process.phase ? ` · ${phases[process.phase] ?? process.phase}` : ""}</p>
      {process.error && <Alert variant="destructive">{process.error.code}: {process.error.message}. Технический сбой не означает непрохождение допуска; прежний принятый профиль может сохраняться.</Alert>}
      <p className="text-xs text-muted-foreground">Исследование принято — это готовность рекомендаций, а не live. Кандидат независимо инициализируется по текущим закрытым свечам, без повторной проверки исторического результата. Сохранённое исследование может относиться к кандидату; при сохранённом принятом поколении среднее и рекомендация относятся к нему. Новые рекомендации поступят после следующего полного исследования слота. Новое поколение действует после watermark закрытой минуты.</p>
      <p className="text-xs text-muted-foreground">Это настроенные пороги, а не история исследований. Очистка сохраняет их настройки. Дата ниже — изменение состояния профиля, включая сброс при очистке, а не дата исследования.</p>
      <div className="overflow-x-auto"><Table aria-label={`Настроенные адаптивные профили · ${algorithmDisplayName(algorithm)}`}>
        <TableHeader><TableRow>{["Минимум сетапов в день", "Исследование / допуск", "Принятое поколение / live", "Кандидат / инициализация live", "Среднее матрицы", "Рекомендация", "Изменение состояния профиля"].map((title) => <TableHead key={title}>{title}</TableHead>)}</TableRow></TableHeader>
        <TableBody>{process.slots.map((slot) => <TableRow key={slot.id}>
          <TableCell>{slot.minSetupsPerDay === 0 ? "0 · без частотного допуска" : slot.minSetupsPerDay}</TableCell>
          <TableCell><div className="grid gap-1"><Badge>{slotStatuses[slot.status] ?? slot.status}</Badge><span className="text-xs">{hasNoProfileResults(slot) ? "Результатов нет" : slot.studyId === null ? "Сохранённое исследование не указано" : `Сохранённое исследование #${slot.studyId}`}</span></div></TableCell>
          <TableCell><SlotIncumbent slot={slot} /></TableCell>
          <TableCell><SlotPending slot={slot} /></TableCell>
          <TableCell>{slot.meanExpectancy === null ? "—" : `${slot.meanExpectancy.toFixed(2)} R`}</TableCell>
          <TableCell>{slot.activeGenerationId !== null && slot.selectedCell ? `${process.config?.study.direction.toUpperCase() ?? ""} · TP ${slot.selectedCell.tpPct}% / SL ${slot.selectedCell.slPct}% · ${slot.selectedCell.rawR.toFixed(2)} R` : "Нет пригодного результата"}</TableCell>
          <TableCell>{slot.updatedAt ? formatIsoDateTime(slot.updatedAt) : "—"}</TableCell>
        </TableRow>)}</TableBody>
      </Table></div>
      {process.slots.length === 0 && <p className="text-sm text-muted-foreground">Настроенных профилей пока нет. Включённое исследование не означает готовый live.</p>}
    </>}
    <div className="flex flex-wrap gap-2">
      <Button type="button" variant="outline" onClick={controller.refresh} disabled={controller.busy !== null}>Обновить статус непрерывного процесса</Button>
      <Button type="button" variant="outline" onClick={() => setOpen(true)} disabled={!controller.available || controller.busy !== null}>Настроить непрерывное исследование</Button>
      {process && process.status !== "stopped" ? <Button type="button" variant="outline" disabled={!controller.available || controller.busy !== null || process.status === "stopping"} onClick={() => { void controller.stop(); }}>{process.status === "stopping" ? "Остановка запрошена · ожидаем drain" : "Остановить непрерывное исследование"}</Button>
        : <Button type="button" disabled={!controller.available || !process?.config || controller.busy !== null} onClick={() => { void controller.start(); }}>Запустить непрерывное исследование</Button>}
      {recoveryExhausted && <Button type="button" variant="outline" disabled={!controller.available || controller.busy !== null} onClick={() => { void controller.start(); }}>Повторить восстановление исследования</Button>}
    </div>
    {recoveryExhausted && <p className="text-xs text-muted-foreground">Повтор восстановления сохраняет тот же ID исследования, сохранённый вход и исходный абсолютный дедлайн, без нового окна. Эта явная команда Start сбрасывает бюджет попыток; возобновление допуска обслуживания его не сбрасывает.</p>}
    <p className="text-xs text-muted-foreground">Остановка общая для алгоритма: деактивирует его адаптивные профили. После drain очищаются только незавершённые дочерние данные; завершённая история и постоянный live сохраняются.</p>
    <p className="text-xs text-muted-foreground">R рассчитан только по наблюдавшимся закрытым TP/SL-сделкам. Правый край последних N суток может быть незрелым: будущие входы, незакрытые сделки и пропуски не подменяются нулевой доходностью или SL. Частота eligible-входов и доходность — разные показатели.</p>
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogContent className="max-w-4xl">
        <DialogHeader><DialogTitle>Непрерывное исследование · {algorithmDisplayName(algorithm)}</DialogTitle><DialogDescription>Метаданные и домены как в ручной оптимизации. Даты и зрелость первого сетапа фиксирует сервер.</DialogDescription></DialogHeader>
        <div className="grid min-h-0 min-w-0 gap-4 overflow-y-auto pr-1">{open && <ContinuousForm algorithm={algorithm} controller={controller} onSaved={() => setOpen(false)} />}</div>
      </DialogContent>
    </Dialog>
  </section>;
}
