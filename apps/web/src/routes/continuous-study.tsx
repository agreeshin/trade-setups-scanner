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
import { formatIsoDateTime } from "../lib/datetime-format";
import { fetchPlatform } from "../lib/platform-api";
import { MAX_CONTINUOUS_LOOKBACK_DAYS, MIN_CONTINUOUS_LOOKBACK_DAYS } from "../lib/continuous-api";
import { getOptimizationDraft } from "../lib/optimization-api";
import { ANALYTICS_BASELINE, ANALYTICS_SEARCH_FIELDS, buildContinuousConfig, initialDomains, initialScope, restoreDomains, type ContinuousDraft, type ScopeDraft } from "./optimization-form";
import { OptimizationFields } from "./optimization-fields";
import type { CatalogInstrument } from "./instrument-picker";
import { algorithmDisplayName, errorText, type Algorithm } from "./platform-controls";
import type { ContinuousController } from "./use-continuous-study";
import { applyOptimizationDraftScope, createOptimizationDraft, restoreOptimizationDraft, useOptimizationDraftSave, type OptimizationFormDraft } from "./optimization-draft";

const statuses = { stopped: "Остановлено", running: "Непрерывное исследование включено", stopping: "Останавливается", error: "Ошибка непрерывного исследования" };
const phases: Record<string, string> = { queued: "Ожидание очереди", idle: "Ожидание следующего исследования", research: "Исследование", acquiring: "Подготовка снимка", replay: "Служебный replay", analytics: "Аналитика сохранённого набора", cleanup: "Очистка незавершённых данных" };
const slotStatuses: Record<string, string> = { idle: "Ожидает исследования", active: "Профиль принят", inactive: "Профиль неактивен", pending: "Ожидает исследования", running: "Подготовка поколения", researching: "Исследование", error: "Техническая ошибка", failed: "Техническая ошибка", nonpass: "Допуск не пройден", no_winner: "Нет оценимого победителя", stopped: "Остановлен" };

export function AdaptiveLive({ controller }: { controller: ContinuousController }) {
  const savedPermission = controller.process?.config?.adaptiveEnabled ?? false;
  const [draft, setDraft] = useState<boolean | null>(null);
  const [saved, setSaved] = useState(false);
  const enabled = draft ?? savedPermission;
  const accepted = controller.process?.slots.filter((slot) => slot.activeGenerationId !== null).length ?? 0;
  return <div className="grid gap-3">
    <label className="flex items-center gap-2 text-sm"><Checkbox checked={enabled} disabled={!controller.available || controller.busy !== null} onCheckedChange={(checked) => { setDraft(checked === true); setSaved(false); }} />Разрешить адаптивное live-сканирование</label>
    <p className="text-sm text-muted-foreground">Отдельное разрешение; исследование не включает live автоматически. Постоянные параметры не меняются. Готовность данных и общий выключатель live проверяются отдельно.</p>
    <p role="status" className="text-sm">{!controller.available ? "Состояние адаптивного live не подтверждено." : !savedPermission ? "Адаптивный live не разрешён." : accepted === 0 ? "Адаптивный live разрешён · ожидание пригодного профиля." : `Адаптивный live разрешён · принято профилей: ${accepted}.`}</p>
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
  return <section aria-label="Непрерывные исследования" className="grid min-w-0 gap-3">
    <h5 className="font-semibold">Непрерывные исследования</h5>
    {controller.loading && <Spinner label="Проверка непрерывного процесса" />}
    {controller.error && <Alert variant="destructive" role="alert">{controller.error}</Alert>}
    {!controller.available && process && <p className="text-sm">Показан последний подтверждённый снимок; актуальность не подтверждена.</p>}
    {controller.available && <p role="status" className="text-sm">{process ? statuses[process.status] : "Процесс не настроен. Исследование не запущено."}</p>}
    {process && <>
      <p className="text-sm">Ревизия конфигурации: {process.configRevision}{process.activeStudyId !== null ? ` · Исследование #${process.activeStudyId}` : ""}{process.phase ? ` · ${phases[process.phase] ?? process.phase}` : ""}</p>
      {process.error && <Alert variant="destructive">{process.error.code}: {process.error.message}. Технический сбой не означает непрохождение допуска; прежний принятый профиль может сохраняться.</Alert>}
      <div className="overflow-x-auto"><Table aria-label={`Адаптивные профили · ${algorithmDisplayName(algorithm)}`}>
        <TableHeader><TableRow>{["Минимум сетапов в день", "Состояние", "Поколение / исследование", "Среднее матрицы", "Рекомендация", "Обновлено"].map((title) => <TableHead key={title}>{title}</TableHead>)}</TableRow></TableHeader>
        <TableBody>{process.slots.map((slot) => <TableRow key={slot.id}>
          <TableCell>{slot.minSetupsPerDay === 0 ? "0 · без частотного допуска" : slot.minSetupsPerDay}</TableCell>
          <TableCell><Badge>{slotStatuses[slot.status] ?? slot.status}</Badge>{slot.activeGenerationId !== null && <span className="block text-xs">Принятое поколение сохранено</span>}</TableCell>
          <TableCell>{slot.activeGenerationId === null ? "—" : `#${slot.activeGenerationId}`} / {slot.studyId === null ? "—" : `#${slot.studyId}`}</TableCell>
          <TableCell>{slot.meanExpectancy === null ? "—" : `${slot.meanExpectancy.toFixed(2)} R`}</TableCell>
          <TableCell>{slot.activeGenerationId !== null && slot.selectedCell ? `${process.config?.study.direction.toUpperCase() ?? ""} · TP ${slot.selectedCell.tpPct}% / SL ${slot.selectedCell.slPct}% · ${slot.selectedCell.rawR.toFixed(2)} R` : "Нет пригодного результата"}</TableCell>
          <TableCell>{slot.updatedAt ? formatIsoDateTime(slot.updatedAt) : "—"}</TableCell>
        </TableRow>)}</TableBody>
      </Table></div>
      {process.slots.length === 0 && <p className="text-sm text-muted-foreground">Принятых профилей пока нет. Включённое исследование не означает готовый live.</p>}
    </>}
    <div className="flex flex-wrap gap-2">
      <Button type="button" variant="outline" onClick={controller.refresh} disabled={controller.busy !== null}>Обновить статус непрерывного процесса</Button>
      <Button type="button" variant="outline" onClick={() => setOpen(true)} disabled={!controller.available || controller.busy !== null}>Настроить непрерывное исследование</Button>
      {process && process.status !== "stopped" ? <Button type="button" variant="outline" disabled={!controller.available || controller.busy !== null || process.status === "stopping"} onClick={() => { void controller.stop(); }}>{process.status === "stopping" ? "Остановка запрошена · ожидаем drain" : "Остановить непрерывное исследование"}</Button>
        : <Button type="button" disabled={!controller.available || !process?.config || controller.busy !== null} onClick={() => { void controller.start(); }}>Запустить непрерывное исследование</Button>}
    </div>
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
