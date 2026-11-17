import { useEffect, useId, useRef, useState, type FormEvent, type ReactNode } from "react";
import { Play, RefreshCw, Settings, Square } from "lucide-react";
import type { AdaptiveProfilesController } from "./use-adaptive-profiles";
import type { AdaptiveProfile } from "../lib/adaptive-profiles-api";
import { Alert } from "../components/ui/alert";
import { Badge, type BadgeVariant } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import { Checkbox } from "../components/ui/checkbox";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "../components/ui/dialog";
import { Field } from "../components/ui/field";
import { Input } from "../components/ui/input";
import { InfoNote } from "../components/ui/info-note";
import { IconButton } from "../components/ui/icon-button";
import { Tooltip } from "../components/ui/tooltip";
import { Spinner } from "../components/ui/spinner";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "../components/ui/table";
import { effectiveTimezone, formatIsoDateTime, formatUnix } from "../lib/datetime-format";
import { fetchPlatform } from "../lib/platform-api";
import { MAX_CONTINUOUS_LOOKBACK_DAYS, MIN_CONTINUOUS_LOOKBACK_DAYS, type ContinuousProcess, type ContinuousSlot, type LiveReadinessSummary } from "../lib/continuous-api";
import { getOptimizationDraft } from "../lib/optimization-api";
import { restoreForecastRecipeDraft } from "../lib/forecast-recipe";
import { ANALYTICS_BASELINE, ANALYTICS_SEARCH_FIELDS, buildContinuousConfig, initialDomains, initialScope, restoreDomains, type ContinuousDraft, type ScopeDraft } from "./optimization-form";
import { OptimizationFields } from "./optimization-fields";
import type { CatalogInstrument } from "./instrument-picker";
import { algorithmDisplayName, errorText, type Algorithm } from "./platform-controls";
import type { ContinuousController } from "./use-continuous-study";
import { applyOptimizationDraftScope, createOptimizationDraft, restoreOptimizationDraft, useOptimizationDraftSave, type OptimizationFormDraft } from "./optimization-draft";

const statuses = { stopped: "Остановлено", running: "Непрерывное исследование включено", stopping: "Останавливается", error: "Ошибка непрерывного исследования" };
const phases: Record<string, string> = { queued: "Ожидание очереди", idle: "Ожидание следующего исследования", research: "Исследование", acquiring: "Подготовка снимка", replay: "Служебный replay", analytics: "Аналитика сохранённого набора", cleanup: "Очистка незавершённых данных" };
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

function invalidReadinessCounts(summary: LiveReadinessSummary | null | undefined): boolean {
  if (!summary) return false;
  const counts = [summary.totalInstruments, summary.readyInstruments, summary.waitingInstruments, summary.otherWaitingInstruments, ...summary.waitingReasons.map((entry) => entry.count)];
  return summary.totalInstruments <= 0 || counts.some((count) => !Number.isSafeInteger(count) || count < 0)
    || summary.readyInstruments + summary.waitingInstruments !== summary.totalInstruments
    || summary.waitingReasons.reduce((total, entry) => total + entry.count, summary.otherWaitingInstruments) !== summary.waitingInstruments;
}

function SlotReadiness({ slot, invalid }: { slot: ContinuousSlot; invalid: boolean }) {
  const summary = slot.liveReadinessSummary;
  if (!summary || invalid || slot.activeGenerationId === null || slot.activeStatus === "inactive" || slot.activeStatus === "error" || summary.generationId !== slot.activeGenerationId) return <span>Готовность инструментов неизвестна.</span>;
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

function liveStatus(slot: ContinuousSlot): { label: string; variant: BadgeVariant } {
  if (["error", "failed"].includes(slot.status) || slot.pendingStatus === "error" || slot.pendingError != null || slot.activeStatus === "error") return { label: "Ошибка", variant: "danger" };
  if (slot.activeGenerationId !== null && slot.activeStatus === "active") return { label: "Активно", variant: "success" };
  if (slot.status === "stopping") return { label: "Останавливается", variant: "warning" };
  if (slot.status === "stopped") return { label: "Остановлен", variant: "warning" };
  if (slot.pendingStatus === "warming" || slot.status === "warming") return { label: "Идёт инициализация", variant: "warning" };
  if (slot.pendingStatus != null || slot.pendingGenerationId != null) return { label: "Ожидает", variant: "warning" };
  if ((slot.activeGenerationId !== null && slot.activeStatus === "inactive") || ["inactive", "nonpass", "rejected", "no_winner"].includes(slot.status)) return { label: "Неактивно", variant: "warning" };
  if (slot.activeGenerationId !== null) return { label: "Состояние live неизвестно (старый API)", variant: "warning" };
  if (["idle", "pending", "running", "researching", "queued", "research", "acquiring", "replay", "analytics", "cleanup"].includes(slot.status)) return { label: "Ожидает", variant: "warning" };
  return { label: "Нет действующего поколения", variant: "warning" };
}

function liveProblems(slot: ContinuousSlot, process: Pick<ContinuousProcess, "status" | "error"> | null, available: boolean, invalidCounts: boolean): string[] {
  // Render-local, at most seven bounded descriptions; no diagnostic history is retained.
  const problems: string[] = [];
  if (!available) problems.push("Актуальность снимка не подтверждена. Показано последнее известное состояние live.");
  if (process && (process.status === "error" || process.error !== null)) problems.push(`Ошибка непрерывного процесса (status=${process.status}): ${pendingErrorMessage(process.error) ?? "Описание ошибки не передано."}`);
  if (["error", "failed"].includes(slot.status)) problems.push(`Техническая ошибка профиля (status=${slot.status}).`);
  if (slot.activeStatus === "error") problems.push("Ошибка действующего live (activeStatus=error).");
  if (slot.pendingStatus === "error" || slot.pendingError != null) problems.push(`Ошибка инициализации live${slot.pendingStatus === "error" ? " (pendingStatus=error)" : ""}: ${pendingErrorMessage(slot.pendingError) ?? "Описание ошибки не передано."}`);
  const summary = slot.liveReadinessSummary;
  if (summary && slot.activeGenerationId !== null && summary.generationId !== slot.activeGenerationId) problems.push(`Сводка готовности относится к поколению #${summary.generationId} вместо действующего поколения #${slot.activeGenerationId}.`);
  if (summary && invalidCounts) problems.push(`Некорректные числа готовности: всего ${summary.totalInstruments}, готовы ${summary.readyInstruments}, ожидают ${summary.waitingInstruments}; counts или сумма причин ожидания несогласованы.`);
  return problems;
}

function SlotIncumbent({ slot, process, available }: { slot: ContinuousSlot; process: Pick<ContinuousProcess, "status" | "error"> | null; available: boolean }) {
  const absent = slot.activeGenerationId === null;
  const status = liveStatus(slot);
  const invalidCounts = invalidReadinessCounts(slot.liveReadinessSummary);
  const problems = liveProblems(slot, process, available, invalidCounts);
  return <Tooltip content={<div className="grid gap-2">
    {problems.length > 0 && <Alert variant="destructive" role="alert" className="break-words">
      <ul aria-label="Проблемы адаптивного live" className="grid gap-1">{problems.map((problem) => <li key={problem}>{problem}</li>)}</ul>
    </Alert>}
    {absent ? <span>Адаптивные сигналы этого профиля не формируются.</span> : <>
      <span>{slot.activeStudyId == null ? "Исследование действующего поколения неизвестно" : `Источник метрик и рекомендации: исследование #${slot.activeStudyId} · поколение #${slot.activeGenerationId}`}</span>
      <span>Принятое поколение сохранено</span>
    </>}
    <SlotReadiness slot={slot} invalid={invalidCounts} />
    {!absent && <span>{slot.liveWatermark == null ? "Watermark переключения неизвестен" : `Watermark переключения: ${closedTime(slot.liveWatermark)}`}</span>}
  </div>}>
    <button type="button" className="inline-flex cursor-help flex-wrap items-center gap-1 rounded-lg text-left focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
      {!absent ? <span>{`${slot.activeStatus === "active" ? "Действующее" : "Принятое"} поколение #${slot.activeGenerationId}`}</span>
        : status.label !== "Нет действующего поколения" && <span>Нет действующего поколения</span>}
      <Badge variant={status.variant} className="whitespace-normal text-left">{status.label}</Badge>
    </button>
  </Tooltip>;
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
    {slot.pendingStudyId != null && <span>Исследование кандидата #{slot.pendingStudyId}</span>}
    {absent ? <span>Нет кандидата на замену</span> : <Badge variant={slot.pendingStatus === "error" ? "danger" : "neutral"}>{legacy ? "Статус инициализации live неизвестен (старый API)" : slot.pendingStatus ? pendingStatuses[slot.pendingStatus] ?? `Состояние кандидата: ${slot.pendingStatus.slice(0, 120)}` : "Статус инициализации live неизвестен"}</Badge>}
    {!legacy && !absent && <span>{slot.readinessFrontier === undefined ? "Граница готовности неизвестна" : slot.readinessFrontier === null ? "Граница готовности ещё не подтверждена" : `Граница готовности (закрытие минуты): ${closedTime(slot.readinessFrontier)}`}</span>}
    {error && <p role="alert" className="text-destructive">Ошибка кандидата: {error}</p>}
  </div>;
}

function SlotResearch({ slot }: { slot: ContinuousSlot }) {
  return <div className="grid gap-1 text-xs">
    <Badge>Исследование принято</Badge>
    <span>{slot.studyId === null ? "Исследование принятой версии неизвестно" : `Сохранённое исследование #${slot.studyId}`}</span>
    {slot.finalizedGenerationId != null && <span>Полностью завершённое поколение #{slot.finalizedGenerationId}</span>}
  </div>;
}


function SlotMatrixMean({ slot }: { slot: ContinuousSlot }) {
  if (slot.meanExpectancy === null) return <>—</>;
  const forecastShare = slot.trendMode === "blend" && slot.trendBlendLambda != null ? Math.round(slot.trendBlendLambda * 100) : null;
  const marker = forecastShare === null ? slot.trendMode : `blend · ${100 - forecastShare}/${forecastShare}`;
  return <>{`${slot.meanExpectancy.toFixed(2)} R`}{marker != null && <span className="text-xs text-muted-foreground">{` · ${marker}`}</span>}</>;
}

export function AdaptiveLive({ controller }: { controller: AdaptiveProfilesController }) {
  const savedPermission = controller.permission ?? false;
  const [draft, setDraft] = useState<boolean | null>(null);
  const [saved, setSaved] = useState(false);
  const enabled = draft ?? savedPermission;
  const accepted = controller.page?.profiles.filter((profile) => profile.activeStatus === "active").length ?? 0;
  return <div className="grid gap-3">
    <div className="flex items-center gap-2"><label className="flex items-center gap-2 text-sm"><Checkbox checked={enabled} disabled={!controller.available || controller.permissionBusy} onCheckedChange={(checked) => { setDraft(checked === true); setSaved(false); }} />Разрешить адаптивное live-сканирование</label>
    <InfoNote>Отдельное разрешение; исследование не включает live автоматически. Постоянные параметры не меняются. Готовность данных и общий выключатель live проверяются отдельно.</InfoNote></div>
    <p role="status" className="text-sm">{!controller.available ? "Состояние адаптивного live не подтверждено." : !savedPermission ? "Адаптивный live не разрешён." : accepted === 0 ? "Адаптивный live разрешён · действующих профилей нет, ожидается инициализация." : `Адаптивный live разрешён · принято профилей: ${accepted}.`}</p>
    <div><Button type="button" variant="outline" disabled={!controller.available || controller.permissionBusy || enabled === savedPermission} onClick={() => {
      void controller.savePermission(enabled).then((ok) => { if (ok) { setDraft(null); setSaved(true); } });
    }}>Сохранить разрешение адаптивного live</Button></div>
    {saved && <p role="status" className="text-sm">Разрешение адаптивного live сохранено.</p>}
  </div>;
}

export function AdaptiveProfiles({ algorithm, controller, research }: { algorithm: Algorithm; controller: AdaptiveProfilesController; research?: ContinuousController }) {
  const profiles = controller.page?.profiles ?? [];
  return <div className="grid min-w-0 gap-3">
    <div className="flex items-center gap-2"><h5 className="font-semibold">Адаптивные профили</h5><InfoNote label="О настроенных адаптивных профилях">Исследование принято — это готовность рекомендаций, а не live. Кандидат инициализируется независимо, без повторной проверки исторического результата. Сохранённое исследование относится к последнему полностью завершённому поколению после replay, аналитики и gate. Дата ниже — изменение состояния профиля, не дата исследования. Stop исследования и удаление порога из очереди сохраняют самостоятельный профиль.</InfoNote></div>
    {controller.loading && <Spinner label="Проверка самостоятельных профилей" />}
    {controller.error && <Alert variant="destructive" role="alert">{controller.error}</Alert>}
    {!controller.available && controller.page && <p className="text-sm">Показан последний подтверждённый снимок профилей; актуальность не подтверждена.</p>}
    <div><IconButton label="Обновить статус адаптивных профилей" icon={RefreshCw} onClick={controller.refresh} /></div>
    {controller.page && <>
      <div className="overflow-x-auto"><Table aria-label={`Адаптивные профили · ${algorithmDisplayName(algorithm)}`}>
        <TableHeader><TableRow>{["Минимум сетапов в день", "Принятый результат", "Принятое поколение / live", "Кандидат / инициализация live", "Среднее матрицы", "Рекомендация", "Изменение состояния профиля", "Управление профилем"].map((title) => <TableHead key={title}>{title}</TableHead>)}</TableRow></TableHeader>
        <TableBody>{profiles.map((profile) => <TableRow key={profile.id}>
          <TableCell>{profile.minSetupsPerDay === 0 ? "0 · без частотного допуска" : profile.minSetupsPerDay}{!profile.inResearchQueue && <p className="text-xs text-muted-foreground">Вне исследовательской очереди</p>}</TableCell>
          <TableCell><SlotResearch slot={{ ...profile, finalizedGenerationId: profile.acceptedGenerationId, runningStudyId: null }} /></TableCell>
          <TableCell><SlotIncumbent slot={profile} process={research?.process ?? null} available={controller.available} /></TableCell>
          <TableCell><SlotPending slot={profile} /></TableCell>
          <TableCell><SlotMatrixMean slot={profile} /></TableCell>
          <TableCell>{profile.activeGenerationId !== null && profile.selectedCell ? <>{`${profile.direction?.toUpperCase() ?? ""} · TP ${profile.selectedCell.tpPct}% / SL ${profile.selectedCell.slPct}% · ${profile.selectedCell.rawR.toFixed(2)} R`}{profile.selectedCell.forecastR !== undefined && <p className="text-xs">Выбор по прогнозу: {profile.selectedCell.forecastR.toFixed(2)} R · score матрицы {profile.selectedCell.forecastMatrixR?.toFixed(2) ?? "—"} R</p>}</> : "Нет пригодного результата"}</TableCell>
          <TableCell>{profile.updatedAt ? formatIsoDateTime(profile.updatedAt) : "—"}</TableCell>
          <TableCell><ProfileControl profile={profile} controller={controller} /></TableCell>
        </TableRow>)}</TableBody>
      </Table></div>
      {profiles.length === 0 && <p className="text-sm text-muted-foreground">Принятых адаптивных профилей пока нет. Настройка исследования сама по себе профиль не создаёт.</p>}
      <p className="text-xs text-muted-foreground">Всего профилей: {controller.page.total} · квоты: всего {controller.page.limits.totalProfiles}, active/warming {controller.page.limits.runningProfiles}.</p>
      {(controller.page.total > 50 || controller.offset > 0) && <div className="flex gap-2"><Button type="button" variant="outline" disabled={controller.offset === 0 || !controller.available} onClick={() => controller.setOffset(Math.max(0, controller.offset - 50))}>Предыдущие профили</Button><Button type="button" variant="outline" disabled={controller.offset + 50 >= controller.page.total || !controller.available} onClick={() => controller.setOffset(controller.offset + 50)}>Следующие профили</Button></div>}
      {controller.page.blockedHandoffs.map((handoff) => <Alert key={handoff.generationId} variant="destructive" role="alert">Результат поколения #{handoff.generationId} сохранён, профиль не допущен: {pendingErrorMessage(handoff.error)}</Alert>)}
    </>}
  </div>;
}
function ProfileControl({ profile, controller }: { profile: AdaptiveProfile; controller: AdaptiveProfilesController }) {
  const action = profile.desiredRunning ? "stop" : "play";
  const blocked = profile.blockingReason !== null && action === "play";
  const label = `${action === "stop" ? "Остановить" : "Запустить"} адаптивный профиль ${profile.id}`;
  return <div className="grid gap-1" aria-busy={controller.busy[profile.id] !== undefined}>
    <IconButton label={label} icon={action === "stop" ? Square : Play} disabled={!controller.available || controller.busy[profile.id] !== undefined || profile.status === "stopping" || blocked} onClick={() => { void controller.command(profile, action); }} />
    {profile.desiredRunning && profile.initializationStatus === "exhausted" && <IconButton label={`Повторить запуск адаптивного профиля ${profile.id}`} icon={Play} disabled={!controller.available || controller.busy[profile.id] !== undefined || profile.status === "stopping" || profile.blockingReason !== null} onClick={() => { void controller.command(profile, "play"); }} />}
    {profile.status === "stopping" && <span className="text-xs">Останавливается · ожидаем drain</span>}
    {profile.initializationStatus === "exhausted" && <span className="text-xs">Попытки исчерпаны: {profile.pendingAttempts}/{profile.attemptLimit}. Play выдаст ещё 3.</span>}
    {profile.blockingReason && <span className="text-xs">{profile.blockingReason === "global_live_disabled" ? "Общий live выключен" : profile.blockingReason === "adaptive_permission_disabled" ? "Адаптивный live не разрешён" : profile.blockingReason.slice(0, 120)}</span>}
    {controller.rowErrors[profile.id] && <Alert variant="destructive" role="alert">{controller.rowErrors[profile.id]}</Alert>}
  </div>;
}

function ContinuousForm({ algorithm, controller, onSaved }: { algorithm: Algorithm; controller: ContinuousController; onSaved: () => void }) {
  const saved = controller.process?.config;
  const baseline = saved ? { ...algorithm, params: saved.study.algorithmParams } : algorithm;
  const [scope, setScope] = useState<ScopeDraft>(() => saved ? { ...initialScope(), symbols: saved.study.instruments.map((item) => item.symbol).join("\n"), direction: saved.study.direction,
    horizonMinutes: String(saved.study.horizonMinutes), tpLevels: saved.study.tpLevels.join(", "), slLevels: saved.study.slLevels.join(", "), minClosedPerCell: String(saved.study.minClosedPerCell), seed: String(saved.study.seed), maxTrials: String(saved.study.maxTrials), maxSeconds: String(saved.study.maxSeconds), trendMode: saved.study.trendMode ?? "mean", trendBendDays: saved.study.trendBendDays ?? "linear",
    trendBlendLambda: String(saved.study.trendBlendLambda ?? 0.7), minForecastExpectancy: String(saved.study.minForecastExpectancy ?? 0),
    forecast: restoreForecastRecipeDraft(saved.study.forecast) } : initialScope());
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
    {locked && <Alert>Параметры исследования доступны для чтения. Чтобы изменить период, пороги или домены, остановите процесс и дождитесь drain. Разрешение adaptive live сохраняется отдельно.</Alert>}
    <fieldset disabled={locked || controller.busy !== null} className="grid gap-3 sm:grid-cols-2">
      <legend className="mb-2 font-semibold">Период и отдельные пороговые профили</legend>
      <Field label="Последние N дней" hint="От 1 минуты до 3660 суток, в целых секундах; дробные сутки разрешены. Окно фиксируется сервером при старте.">{(control) => <Input {...control} type="number" min={MIN_CONTINUOUS_LOOKBACK_DAYS} max={MAX_CONTINUOUS_LOOKBACK_DAYS} step="any" value={draft.lookbackDays} onChange={(event) => setDraft({ ...draft, lookbackDays: event.target.value })} />}</Field>
      <Field label="Минимум сетапов в день — список профилей" hint="До 64 уникальных значений через запятую; 0 отключает частотный допуск. Не поисковый домен и не обещание числа live-сообщений.">{(control) => <Input {...control} maxLength={8192} value={draft.minSetupsPerDay} onChange={(event) => setDraft({ ...draft, minSetupsPerDay: event.target.value })} />}</Field>
      <Field label="Минимальное среднее матожидание, R" hint={scope.trendMode === "forecast" || scope.trendMode === "blend" ? "Только для режима mean. Для forecast/blend действует отдельный forward-гейт: score > minForecastExpectancy, default 0. Среднее за период не ограничивает этот гейт." : "Гейт всей неокруглённой mean-матрицы: score >= minMeanExpectancy, не лучшей ячейки. По умолчанию 0.15 R."}>{(control) => <Input {...control} type="number" step="any" disabled={scope.trendMode === "forecast" || scope.trendMode === "blend"} value={draft.minMeanExpectancy} onChange={(event) => setDraft({ ...draft, minMeanExpectancy: event.target.value })} />}</Field>
    </fieldset>
    {!ready && !error && <Spinner label="Загрузка historical-списка" />}
    {(error ?? draftSave.saveError) && <Alert variant="destructive" role="alert">{error ?? draftSave.saveError}{!ready && <Button type="button" variant="outline" onClick={() => setRetry((value) => value + 1)}>Повторить загрузку</Button>}</Alert>}
    <OptimizationFields algorithm={baseline} scope={scope} onScopeChange={(patch) => { setScope((previous) => ({ ...previous, ...patch })); setError(null); }} catalog={catalog} ready={ready} disabled={locked || controller.busy !== null} algorithmDomains={algorithmDomains} onAlgorithmDomainsChange={setAlgorithmDomains} analyticsDomains={analyticsDomains} onAnalyticsDomainsChange={setAnalyticsDomains} continuous />
    {controller.error && <Alert variant="destructive" role="alert">{controller.error}</Alert>}
    <div><Button type="submit" disabled={!ready || locked || !controller.available || controller.busy !== null}>{controller.busy === "save" ? "Сохранение…" : "Сохранить непрерывное исследование"}</Button></div>
  </form>;
}

export function ContinuousResearch({ controller, children }: { algorithm?: Algorithm; controller: ContinuousController; children?: ReactNode }) {
  const process = controller.process;
  return <section aria-label="Непрерывные исследования" className="grid min-w-0 gap-3">
    <div className="flex items-center gap-2"><h4 className="font-semibold">Непрерывные исследования</h4>
      <InfoNote label="Об остановке и расчёте доходности">Stop отменяет только исследовательскую цепочку и очищает её незавершённые данные после drain. Профили и завершённая история сохраняются. Следующий Start создаёт новое исследование; техническое восстановление — отдельное действие. R рассчитан только по наблюдавшимся закрытым TP/SL-сделкам, без вымышленных нулей или SL на правом краю.</InfoNote></div>
    {children}
    {controller.loading && <Spinner label="Проверка непрерывного процесса" />}
    {controller.error && <Alert variant="destructive" role="alert">{controller.error}</Alert>}
    {!controller.available && process && <p className="text-sm">Показан последний подтверждённый снимок; актуальность не подтверждена.</p>}
    {controller.available && <p role="status" className="text-sm">{process ? statuses[process.status] : "Процесс не настроен. Исследование не запущено."}</p>}
    {process && <>
      <p className="text-sm">Ревизия конфигурации: {process.configRevision}{process.activeStudyId !== null ? ` · Исследование #${process.activeStudyId}` : ""}{process.phase ? ` · ${phases[process.phase] ?? process.phase}` : ""}</p>
      {process.error && <Alert variant="destructive">{process.error.code}: {process.error.message}. Технический сбой не означает непрохождение допуска; прежний принятый профиль может сохраняться.</Alert>}
      {process.config && <p className="text-sm">Очередь порогов: {process.config.minSetupsPerDay.join(", ")} · последние {process.config.lookbackDays} суток. Изменение очереди не удаляет адаптивные профили.</p>}
    </>}
  </section>;
}

/** Control block in Continuous Research. Commands here affect research only. */
export function ContinuousControls({ algorithm, controller, children }: { algorithm: Algorithm; controller: ContinuousController; children?: ReactNode }) {
  const [open, setOpen] = useState(false);
  const [stopOpen, setStopOpen] = useState(false);
  const [stopPending, setStopPending] = useState(false);
  const stopRequest = useRef(false);
  const stopTriggerId = useId();
  const stopCancelId = useId();
  const process = controller.process;
  const canStop = controller.available && controller.busy === null && !stopPending && process !== null && process.status !== "stopped" && process.status !== "stopping";
  const recoveryExhausted = process?.status === "error" && process.error?.code === "durable_resume_retry_exhausted"
    && process.config !== null && process.activeStudyId !== null;
  async function confirmStop() {
    if (!canStop || stopRequest.current) return;
    stopRequest.current = true; setStopPending(true);
    try {
      if (await controller.stop()) setStopOpen(false);
    } finally {
      stopRequest.current = false; setStopPending(false);
    }
  }
  return <section aria-label="Управление непрерывным исследованием" className="grid min-w-0 gap-3">
    <div className="flex flex-wrap items-center gap-3">
      <span className="text-sm font-medium">Исследование</span>
      <IconButton label="Обновить статус непрерывного процесса" icon={RefreshCw} onClick={controller.refresh} disabled={controller.busy !== null} />
      <IconButton label="Настроить непрерывное исследование" icon={Settings} onClick={() => setOpen(true)} disabled={!controller.available || controller.busy !== null} />
      {process && process.status !== "stopped" ? <IconButton id={stopTriggerId} label={process.status === "stopping" ? "Остановка запрошена · ожидаем drain" : "Остановить непрерывное исследование"} icon={Square} disabled={!canStop} onClick={() => setStopOpen(true)} />
        : <IconButton id={stopTriggerId} label="Запустить непрерывное исследование" icon={Play} disabled={!controller.available || !process?.config || controller.busy !== null} onClick={() => { void controller.start(); }} />}
      {recoveryExhausted && <Button type="button" variant="outline" disabled={!controller.available || controller.busy !== null} onClick={() => { void controller.retryRecovery(); }}>Повторить восстановление исследования</Button>}
      {children}
    </div>
    {recoveryExhausted && <InfoNote label="О повторе восстановления">Повтор восстановления сохраняет тот же ID исследования, сохранённый вход и исходный абсолютный дедлайн, без нового окна. Отдельная команда recovery/retry разрешает повтор технического восстановления; возобновление допуска обслуживания его не сбрасывает. Start после Stop создаёт новое исследование.</InfoNote>}
    <Dialog open={stopOpen} onOpenChange={(next) => { if (!stopRequest.current) setStopOpen(next); }}>
      <DialogContent className="max-w-2xl" onOpenAutoFocus={(event) => { event.preventDefault(); document.getElementById(stopCancelId)?.focus(); }} onCloseAutoFocus={(event) => { event.preventDefault(); document.getElementById(stopTriggerId)?.focus(); }}>
        <DialogHeader>
          <DialogTitle>Остановить непрерывное исследование?</DialogTitle>
          <DialogDescription>Алгоритм: {algorithmDisplayName(algorithm)}. Остановится только исследовательская цепочка; её незавершённые данные будут очищены после drain. Профили, завершённая история и постоянный live сохраняются.</DialogDescription>
        </DialogHeader>
        {controller.error && <Alert variant="destructive" role="alert">{controller.error}</Alert>}
        {!canStop && !stopPending && <p role="status" className="text-sm">Остановка сейчас недоступна: обновите статус и проверьте состояние исследования.</p>}
        <div className="flex shrink-0 flex-wrap justify-end gap-2" aria-busy={stopPending}>
          <Button id={stopCancelId} type="button" variant="outline" disabled={stopPending} onClick={() => setStopOpen(false)}>Отмена</Button>
          <Button type="button" disabled={!canStop} onClick={() => { void confirmStop(); }}>{stopPending ? "Запрашиваем остановку…" : "Остановить исследование"}</Button>
        </div>
      </DialogContent>
    </Dialog>
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogContent className="max-w-4xl">
        <DialogHeader><div className="flex items-center gap-2"><DialogTitle>Непрерывное исследование · {algorithmDisplayName(algorithm)}</DialogTitle><InfoNote>Сохранение не запускает исследование и не включает live. Сервер фиксирует последние N суток при фактическом старте, без lag; вся цепочка использует один период и снимок. Включённая кластеризация выбирает только первый сетап.</InfoNote></div><DialogDescription>Метаданные и домены как в ручной оптимизации. Даты и зрелость первого сетапа фиксирует сервер.</DialogDescription></DialogHeader>
        <div className="grid min-w-0 gap-4">{open && <ContinuousForm algorithm={algorithm} controller={controller} onSaved={() => setOpen(false)} />}</div>
      </DialogContent>
    </Dialog>
  </section>;
}
