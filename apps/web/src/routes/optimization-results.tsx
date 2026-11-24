import { useId, useRef, useState } from "react";
import { Pause, Play, Square } from "lucide-react";
import { Alert } from "../components/ui/alert";
import { Badge } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import { InfoNote } from "../components/ui/info-note";
import { IconButton } from "../components/ui/icon-button";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "../components/ui/dialog";
import { Spinner } from "../components/ui/spinner";
import { Separator } from "../components/ui/separator";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "../components/ui/table";
import { effectiveTimezone, formatIsoDateTime, formatUnix } from "../lib/datetime-format";
import { canPauseStudy, canPlayStudy, canStopStudy, isActiveStudy, isContinuousStudy, type OptimizationStudy, type OptimizationStudyAction } from "../lib/optimization-api";
import { parameterLabels } from "../lib/parameter-labels";
import { forecastRecipeLabel } from "../lib/forecast-recipe";
import type { Algorithm } from "./platform-controls";
import type { OptimizationController } from "./use-optimization-studies";

const statusLabels: Record<OptimizationStudy["status"], string> = {
  queued: "В очереди", running: "Выполняется", pausing: "Постановка на паузу", paused: "На паузе", cancel_requested: "Отмена запрошена",
  completed: "Завершено", cancelled: "Отменено", failed: "Ошибка исследования",
};
const phaseLabels: Record<string, string> = { acquiring: "Подготовка снимка данных", initial: "Начальное исследование", sensitivity: "Чувствительность", refinement: "Уточнение", cleanup: "Очистка временных данных" };
const reasonLabels = { trial_budget: "Исчерпан бюджет trials", time_budget: "Исчерпан бюджет времени", space_exhausted: "Конечное пространство исчерпано", cancelled: "Явная остановка" };
const expectancyFormatter = new Intl.NumberFormat("en-US", {
  notation: "standard", useGrouping: false, minimumFractionDigits: 2, maximumFractionDigits: 2,
});
const matrixFormatter = new Intl.NumberFormat("en-US", {
  notation: "standard", useGrouping: false, minimumFractionDigits: 4, maximumFractionDigits: 4,
});
// Match the existing Analytics controls, not the optimizer search-coordinate wording.
const analyticsLabels: Record<string, string> = {
  clusterEnabled: "Кластеризовать сетапы", clusterGapMinutes: "Разрыв кластера, ч",
  timeShiftMinutes: "Time-shift, мин", priceChangePct: "Смещение цены, %",
  setupMaturity: "Зрелость сетапа в кластере", horizonMinutes: "Горизонт сделки, мин (фиксирован)",
};

export function StudyStatusBadge({ study }: { study: Pick<OptimizationStudy, "status" | "origin"> }) {
  return <Badge variant={study.status === "failed" ? "danger" : study.status === "completed" ? "success" : "info"}>{study.status === "queued" && !isContinuousStudy(study) ? "Ожидает очередь" : statusLabels[study.status]}</Badge>;
}

export function StudyStatusBadges({ study }: { study: Pick<OptimizationStudy, "status" | "origin"> }) {
  return <>
    <StudyStatusBadge study={study} />
    <Badge>{isContinuousStudy(study) ? "Непрерывное" : "Ручное"}</Badge>
  </>;
}

/** The same server-derived controls in a bounded history row and exact-ID detail. */
export function ManualStudyActions({ study, controller, compact = false }: { study: OptimizationStudyAction; controller: OptimizationController; compact?: boolean }) {
  const [stopTargetId, setStopTargetId] = useState<number | null>(null);
  const stopOpen = stopTargetId !== null;
  const [pauseTargetId, setPauseTargetId] = useState<number | null>(null);
  const pauseOpen = pauseTargetId !== null;
  const [pausePending, setPausePending] = useState(false);
  const pauseRequest = useRef(false);
  const pauseTriggerId = useId(); const pauseCancelId = useId();
  const [stopPending, setStopPending] = useState(false);
  const stopRequest = useRef(false);
  const stopTriggerId = useId(); const cancelId = useId(); const fallbackId = useId();
  const busy = controller.creating || controller.cancelling !== null || controller.deleting !== null || controller.controlling != null || stopPending || pausePending;
  const available = controller.ready && !busy;
  const canStop = available && canStopStudy(study) && (!stopOpen || stopTargetId === study.id);
  const canPause = available && canPauseStudy(study) && (!pauseOpen || pauseTargetId === study.id);
  const requestingPause = controller.controlling?.id === study.id && controller.controlling.action === "pause";
  const requestingPlay = controller.controlling?.id === study.id && controller.controlling.action === "play";
  const suffix = compact ? ` ${study.id}` : "";
  async function confirmStop() {
    if (!canStop || stopRequest.current) return;
    stopRequest.current = true; setStopPending(true);
    try { if (await controller.stop(study) !== false) setStopTargetId(null); }
    finally { stopRequest.current = false; setStopPending(false); }
  }
  async function confirmPause() {
    if (!canPause || pauseRequest.current) return;
    pauseRequest.current = true; setPausePending(true); setPauseTargetId(null);
    try { if (await controller.pause(study) !== false) setPauseTargetId(null); }
    finally { pauseRequest.current = false; setPausePending(false); }
  }
  if (isContinuousStudy(study) || ((!isActiveStudy(study) || study.status === "queued") && !stopOpen && !pauseOpen)) return null;
  return <div id={fallbackId} tabIndex={-1} className="flex flex-wrap items-center gap-2">
    {!isContinuousStudy(study) && <>
      {study.status === "running" && <IconButton id={pauseTriggerId} label={`Приостановить исследование${suffix}`} icon={Pause} filled disabled={!canPause} onClick={() => setPauseTargetId(study.id)} />}
      {study.status === "paused" && <IconButton label={`Возобновить исследование${suffix}`} icon={Play} filled disabled={!available || !canPlayStudy(study)} onClick={() => { void controller.play(study); }} />}
      {isActiveStudy(study) && study.status !== "queued" && <IconButton id={stopTriggerId} label={`Остановить исследование${suffix}`} icon={Square} filled disabled={!canStop} onClick={() => setStopTargetId(study.id)} />}
      {(requestingPause || requestingPlay || study.status === "pausing" || study.status === "cancel_requested") && <div role="status" className="flex items-center gap-2 text-sm text-muted-foreground">
        {(requestingPause || requestingPlay || (study.status === "pausing" && !("error" in study && study.error))) && <Spinner label="Ожидание команды исследования" />}
        <span>{requestingPause ? "Запрашиваем паузу…" : requestingPlay ? "Возвращаем в очередь…" : study.status === "pausing" ? "Постановка на паузу" : "Остановка запрошена"}</span>
      </div>}
    </>}
    <Dialog open={pauseOpen} onOpenChange={(open) => { if (!open && !pauseRequest.current) setPauseTargetId(null); }}>
      <DialogContent onOpenAutoFocus={(event) => { event.preventDefault(); document.getElementById(pauseCancelId)?.focus(); }} onCloseAutoFocus={(event) => {
        event.preventDefault(); const trigger = document.getElementById(pauseTriggerId);
        (trigger && !trigger.matches(":disabled") ? trigger : document.getElementById(fallbackId))?.focus();
      }}>
        <DialogHeader><DialogTitle>Приостановить исследование #{pauseTargetId ?? study.id}?</DialogTitle><DialogDescription>Вычислительное состояние сохранится на безопасной границе, а очередь освободится для других исследований. После возобновления это же исследование продолжится с сохранённым прогрессом. Live и сбор данных не изменятся.</DialogDescription></DialogHeader>
        {controller.error && <Alert variant="destructive" role="alert">{controller.error}</Alert>}
        {!canPause && !pausePending && <p role="status" className="text-sm">Состояние исследования изменилось. Обновите статус перед паузой.</p>}
        <div className="flex flex-wrap justify-end gap-2" aria-busy={pausePending}>
          <Button id={pauseCancelId} type="button" variant="outline" disabled={pausePending} onClick={() => setPauseTargetId(null)}>Отмена</Button>
          <Button type="button" disabled={!canPause} onClick={() => { void confirmPause(); }}>{pausePending ? "Запрашиваем паузу…" : "Приостановить исследование"}</Button>
        </div>
      </DialogContent>
    </Dialog>
    <Dialog open={stopOpen} onOpenChange={(open) => { if (!open && !stopRequest.current) setStopTargetId(null); }}>
      <DialogContent onOpenAutoFocus={(event) => { event.preventDefault(); document.getElementById(cancelId)?.focus(); }} onCloseAutoFocus={(event) => {
        event.preventDefault();
        const trigger = document.getElementById(stopTriggerId);
        (trigger && !trigger.matches(":disabled") ? trigger : document.getElementById(fallbackId))?.focus();
      }}>
        <DialogHeader><DialogTitle>Остановить исследование #{stopTargetId ?? study.id}?</DialogTitle>
          <DialogDescription>Остановка необратима: это исследование нельзя будет возобновить. Полностью оценённый частичный результат сохранится, если он есть. Live, непрерывный процесс и другие исследования не изменятся.</DialogDescription></DialogHeader>
        {controller.error && <Alert variant="destructive" role="alert">{controller.error}</Alert>}
        {!canStop && !stopPending && <p role="status" className="text-sm">Состояние исследования изменилось. Обновите статус перед остановкой.</p>}
        <div className="flex flex-wrap justify-end gap-2" aria-busy={stopPending}>
          <Button id={cancelId} type="button" variant="outline" disabled={stopPending} onClick={() => setStopTargetId(null)}>Отмена</Button>
          <Button type="button" disabled={!canStop} onClick={() => { void confirmStop(); }}>{stopPending ? "Запрашиваем остановку…" : "Остановить исследование"}</Button>
        </div>
      </DialogContent>
    </Dialog>
  </div>;
}

export function OptimizationResults({ study, algorithm, controller, onNew }: { study: OptimizationStudy; algorithm: Algorithm; controller: OptimizationController; onNew?: () => void }) {
  const { progress, spec, result } = study;
  const best = result?.best;
  const pair = study.forecastBest;
  const mode = best?.trendMode ?? spec.trendMode ?? "mean";
  const blend = mode === "blend";
  const forecast = mode !== "mean";
  const lambda = best?.trendBlendLambda ?? spec.trendBlendLambda ?? 0.7;
  const blendLabel = `Смесь · ${100 - Math.round(lambda * 100)}/${Math.round(lambda * 100)}`;
  const bend = best?.trendBendDays ?? spec.trendBendDays ?? "linear";
  const recipe = best?.forecast ?? spec.forecast;
  const recipeLabel = forecastRecipeLabel(recipe, bend);
  const frequencyThreshold = spec.minSetupsPerDay === undefined ? "Гейт не применялся"
    : spec.minSetupsPerDay === 0 ? "0 (гейт отключён)" : String(spec.minSetupsPerDay);
  return <section className="grid min-w-0 gap-4" aria-label="Статус исследования">
    <p className="text-sm" role="status">{phaseLabels[progress.phase] ?? progress.phase}</p>
    <dl className="grid gap-2 text-sm sm:grid-cols-2">
      <div><dt className="text-muted-foreground">Завершено trials / бюджет</dt><dd>{progress.evaluatedCandidates} / {progress.maxTrials}</dd></div>
      <div><dt className="text-muted-foreground">Оценимых кандидатов</dt><dd>{progress.scorableCandidates}</dd></div>
      <div><dt className="text-muted-foreground">Прошло, секунд</dt><dd>{progress.elapsedSeconds}</dd></div>
      <div><dt className="text-muted-foreground">Текущий лучший score</dt><dd>{progress.bestScore === null ? "Ещё нет оценимого кандидата" : `${expectancyFormatter.format(progress.bestScore)} R`}</dd></div>
    </dl>
    <p className="text-xs text-muted-foreground">{spec.algorithmId} · версия {spec.algorithmVersion} · {spec.direction.toUpperCase()}. Период закрытия сигналов [{formatUnix(spec.fromUnix)}, {formatUnix(spec.toUnix)}). Инструментов: {spec.instruments.length}; TP×SL: {spec.tpLevels.length}×{spec.slLevels.length}. Seed: {spec.seed}; лимит времени: {spec.maxSeconds} с.</p>
    <section aria-label="Фиксированные требования исследования">
      <dl className="grid gap-2 text-sm sm:grid-cols-2">
        <div><dt className="text-muted-foreground">Минимум закрытых на ячейку</dt><dd>{spec.minClosedPerCell}</dd></div>
        <div><dt className="text-muted-foreground">Минимум сетапов в день</dt><dd>{frequencyThreshold}</dd></div>
        <div><dt className="text-muted-foreground">Метрика поиска</dt><dd>{forecast ? `${blend ? `${blendLabel} · ` : ""}${recipeLabel}` : "Среднее за период"}</dd></div>
        {recipe && <div><dt className="text-muted-foreground">Сохранённый рецепт прогноза</dt><dd>{recipeLabel}</dd></div>}
        {best?.observationCutoff !== undefined && <div><dt className="text-muted-foreground">Cutoff C результата</dt><dd>{formatUnix(best.observationCutoff)} ({effectiveTimezone()}) · из сохранённого результата, не текущей формы</dd></div>}
        {forecast && <div><dt className="text-muted-foreground">Forward-гейт матрицы</dt><dd>score &gt; {spec.minForecastExpectancy ?? 0} R</dd></div>}
      </dl>
    </section>
    <div className="flex flex-wrap gap-2">
      {isActiveStudy(study) && <Button type="button" variant="outline" onClick={controller.refresh}>Обновить статус</Button>}
      {isActiveStudy(study) && isContinuousStudy(study) && <p className="text-sm">Непрерывное исследование останавливается только общей командой алгоритма.</p>}
      <ManualStudyActions study={study} controller={controller} />
      {onNew && <Button type="button" disabled={!controller.ready || controller.creating} onClick={onNew}>Новое исследование</Button>}
    </div>
    {study.status === "cancel_requested" && <Alert role="status">Сервер завершает текущие задачи и очистку. Остановка ещё не завершена.</Alert>}
    {study.error && <Alert variant="destructive" role="alert">{study.error.code}: {study.error.message}</Alert>}
    {study.status === "failed" && study.result === null && <p className="text-sm">Итог не сохранён. Промежуточный bestScore не восстанавливает потерянного кандидата; автоматического перезапуска нет.</p>}
    {result && <><p className="text-sm">Причина завершения: {reasonLabels[result.reason]}.</p><Separator /></>}
    {result && !best && <Alert variant="warning">Нет оценимого кандидата, удовлетворившего требованиям всех выбранных ячеек и порогу «Минимум закрытых на ячейку»{spec.minSetupsPerDay !== undefined && spec.minSetupsPerDay > 0 ? " и частотному порогу «Минимум сетапов в день»" : ""}. Отсутствующий score не равен нулю.</Alert>}
    {best && <>
      <div className="flex flex-wrap items-center gap-2"><h3 className="font-semibold">Лучший score: {expectancyFormatter.format(best.score)} R</h3>
      <InfoNote label="О лучшем score">{best.partial ? "Частичный итог остановленного исследования." : "Лучший полностью проверенный кандидат в пределах бюджета."} Это не доказанный глобальный оптимум. Score — равное среднее {blend ? "blend R" : forecast ? "прогноза R" : "R"} всех выбранных TP×SL-ячеек, а не выбор лучшей TP/SL-пары.</InfoNote></div>
      {forecast && <p className="text-sm">{blend ? `Выбор по смеси среднего и прогноза (${blendLabel}).` : recipe?.method === "decayed_probability" ? "Выбор по вероятностному прогнозу TP с памятью." : "Выбор по прогнозу тренда."} Среднее всей матрицы за период: {best.meanScore == null ? "Не сохранено" : `${expectancyFormatter.format(best.meanScore)} R`}.{!blend && " Прогноз не является средним за период; ретроспективная оценка не доказывает качество будущих сделок."}</p>}
      <section aria-label="Лучшая forecast-пара" className="grid gap-2">
        <div className="flex flex-wrap items-center gap-2"><h4 className="font-medium">Лучшая forecast-пара</h4><InfoNote label="О лучшей forecast-паре">Максимальный чистый прогноз ячейки сохранённого победителя, не score всей матрицы. Среднее и TP/SL относятся к этой же ячейке.</InfoNote></div>
        {pair == null ? <p className="text-sm">—</p> : <dl className="grid gap-2 text-sm sm:grid-cols-3">
          <div><dt className="text-muted-foreground">Прогноз матожидания</dt><dd>{expectancyFormatter.format(pair.forecastR)} R</dd></div>
          <div><dt className="text-muted-foreground">Среднее матожидание этой ячейки</dt><dd>{expectancyFormatter.format(pair.meanR)} R</dd></div>
          <div><dt className="text-muted-foreground">TP / SL</dt><dd>{pair.tpPct}% / {pair.slPct}%</dd></div>
        </dl>}
      </section>
      <Separator />
      <section aria-label="Полные параметры результата" className="grid gap-2">
        <h4 className="font-medium">Полные параметры алгоритма</h4>
        <dl className="grid gap-2 text-sm sm:grid-cols-2">{Object.entries(best.params).map(([key, value]) => {
          const field = algorithm.fields.find((item) => item.key === key);
          const label = field ? `${field.label} (${field.unit})` : parameterLabels[key] ?? key;
          return <div key={key} className="break-words"><dt className="text-muted-foreground">{label}</dt><dd>{String(value)}</dd></div>;
        })}</dl>
        <h4 className="font-medium">Эффективная аналитика</h4>
        <dl className="grid gap-2 text-sm sm:grid-cols-2">{Object.entries({ ...best.analytics, setupMaturity: best.analytics.clusterEnabled ? spec.setupMaturity : null, horizonMinutes: spec.horizonMinutes }).map(([key, value]) => <div key={key}><dt className="text-muted-foreground">{analyticsLabels[key] ?? key}</dt><dd>{String(key === "clusterGapMinutes" && typeof value === "number" ? value / 60 : value)}</dd></div>)}</dl>
      </section>
      <section aria-label="Поддержка результата">
        <dl className="grid gap-2 text-sm sm:grid-cols-2">
          <div><dt className="text-muted-foreground">Выбрано сетапов</dt><dd>{best.support?.selectedSetupCount ?? "Не сохранено"}</dd></div>
          {typeof best.support?.avgSetupsPerDay === "number" && <div><dt className="text-muted-foreground">Среднее сетапов в день</dt><dd>{best.support.avgSetupsPerDay}</dd></div>}
        </dl>
      </section>
      <details><summary className="cursor-pointer font-medium">Матрица и поддержка результата</summary>
      <div className="mt-2 overflow-x-auto">
        <Table aria-label="Матрица и поддержка результата">
          <TableHeader><TableRow>{["TP %", "SL %", blend ? "Blend R" : forecast ? "Прогноз R" : "R", ...(forecast ? ["Среднее R за период"] : []), ...(blend ? ["Чистый прогноз R"] : []), "Среднее %", "Закрытых", "TP", "SL", "Не закрыты", "Нет данных", "Гейт исключил"].map((title) => <TableHead key={title} numeric>{title}</TableHead>)}</TableRow></TableHeader>
          <TableBody>{best.matrix.map((cell) => <TableRow key={`${cell.tpPct}:${cell.slPct}`}>
            {[cell.tpPct, cell.slPct, cell.riskExpectancy, ...(forecast ? [cell.meanRiskExpectancy] : []), ...(blend ? [cell.forecastRiskExpectancy] : []), cell.avgMovePct].map((value, index) => <TableCell key={index} numeric>{value == null ? "—" : matrixFormatter.format(value)}</TableCell>)}
            {[cell.closedCount, cell.tpCount, cell.slCount, cell.unclosedCount, cell.noDataCount, cell.priceChangeFilteredCount].map((value, index) => <TableCell key={index} numeric>{value ?? "—"}</TableCell>)}
          </TableRow>)}</TableBody>
        </Table>
      </div>
      </details>
      <dl className="grid gap-2 text-xs sm:grid-cols-2">{Object.entries(best.provenance).map(([key, value]) => <div className="break-all" key={key}><dt className="text-muted-foreground">{key}</dt><dd>{String(value)}</dd></div>)}</dl>
      <ul className="grid gap-1 text-sm">{best.conclusions.slice(0, 32).map((item, index) => <li className="break-words" key={index}>{item.slice(0, 1000)}</li>)}</ul>
    </>}
    <details><summary className="cursor-pointer font-medium">Все входы исследования</summary><pre aria-label="Сохранённый spec" className="mt-2 max-h-96 overflow-auto whitespace-pre-wrap break-all rounded-md bg-muted p-3 text-xs">{JSON.stringify(spec, null, 2)}</pre></details>
    <section aria-label="Главные события исследования" className="grid gap-2">
      <h4 className="font-medium">Главные события · последние {Math.min(study.events.length, 50)}</h4>
      <ol className="grid max-h-64 gap-2 overflow-y-auto text-xs">{study.events.slice(-50).map((event, index) => <li key={`${event.at}:${index}`} className="break-words">{formatIsoDateTime(event.at)} · {event.kind} · {event.message.slice(0, 1000)}</li>)}</ol>
    </section>
  </section>;
}
