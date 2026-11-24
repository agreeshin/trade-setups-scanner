import { useEffect, useId, useRef, useState, useSyncExternalStore } from "react";
import { Alert } from "../components/ui/alert";
import { Badge } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "../components/ui/dialog";
import { Pagination } from "../components/ui/pagination";
import { Separator } from "../components/ui/separator";
import { Spinner } from "../components/ui/spinner";
import { Switch } from "../components/ui/switch";
import { formatIsoDateTime, interfaceSettings, subscribeInterfaceSettings } from "../lib/datetime-format";
import { forecastRecipeLabel } from "../lib/forecast-recipe";
import { getStudyActualSetups, isStudyActualPending, STUDY_ACTUAL_PAGE_SIZE, StudyActualApiError, type StudyActualDetail, type StudyActualSetupPage } from "../lib/study-actual-api";
import { errorText } from "./platform-controls";
import { formatOutcomeRisk, outcomeMeanClass, SetupOutcomesTable } from "./setup-outcomes-table";
import { StudyActualChart } from "./study-actual-chart";
import { useStudyActualDetail } from "./use-study-actual-detail";

function ActualSetupsDialog({ actual, onClose, onInvalidated, returnFocus }: { actual: StudyActualDetail; onClose: () => void; onInvalidated: () => void; returnFocus: () => void }) {
  const [offset, setOffset] = useState(0);
  const [hideGateFailed, setHideGateFailed] = useState(true);
  const [page, setPage] = useState<{ identity: string; value: StudyActualSetupPage } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [retry, setRetry] = useState(0);
  const identity = `${actual.studyId}:${actual.revision}:${actual.jobId}:${offset}:${hideGateFailed}`;
  const visible = page?.identity === identity ? page.value : null;
  useEffect(() => {
    let request: AbortController | undefined;
    const load = () => {
      request?.abort(); setPage(null); setError(null);
      if (document.visibilityState === "hidden") return;
      const controller = new AbortController(); request = controller;
      void getStudyActualSetups(actual.studyId, actual.revision, actual.jobId!, offset, controller.signal, hideGateFailed).then((next) => {
        if (!controller.signal.aborted) setPage({ identity, value: next });
      }).catch((cause) => {
        if (controller.signal.aborted) return;
        if (cause instanceof StudyActualApiError && cause.code === "study_actual_revision_changed") { onInvalidated(); return; }
        setError(errorText(cause));
      });
    };
    const visibility = () => { request?.abort(); if (document.visibilityState !== "hidden") load(); };
    load(); document.addEventListener("visibilitychange", visibility);
    return () => { request?.abort(); document.removeEventListener("visibilitychange", visibility); };
  }, [actual.studyId, actual.revision, actual.jobId, offset, hideGateFailed, identity, retry, onInvalidated]);
  return <Dialog open onOpenChange={(open) => { if (!open) onClose(); }}>
    <DialogContent className="max-w-[96rem]" onCloseAutoFocus={(event) => { event.preventDefault(); returnFocus(); }}>
      <DialogHeader><DialogTitle>Сетапы после исследования #{actual.studyId}</DialogTitle><DialogDescription>Новое историческое воспроизведение · ревизия {actual.revision}. Только сохранённый набор после фактического старта исследования, не выпущенные live-уведомления.</DialogDescription></DialogHeader>
      <div className="flex justify-end">
        <label className="flex items-center gap-2 text-sm text-muted-foreground"><Switch checked={hideGateFailed} onCheckedChange={(checked) => { setHideGateFailed(checked); setOffset(0); }} /> Скрыть сетапы, которые не прошли гейт</label>
      </div>
      {error && <Alert variant="destructive" role="alert">{error}<Button type="button" variant="outline" onClick={() => setRetry((value) => value + 1)}>Повторить загрузку сетапов</Button></Alert>}
      {!visible && !error && <Spinner label="Загрузка сетапов после исследования" />}
      {visible && <>
        <SetupOutcomesTable label="Сетапы после исследования" setups={visible.setups} />
        {visible.total === 0 && <p className="text-sm text-muted-foreground">{hideGateFailed ? "Нет сетапов, соответствующих фильтру." : "Нет сетапов в периоде."}</p>}
        <Pagination page={Math.floor(offset / STUDY_ACTUAL_PAGE_SIZE) + 1} pages={Math.max(1, Math.ceil(visible.total / STUDY_ACTUAL_PAGE_SIZE))}
          onPrev={() => setOffset(Math.max(0, offset - STUDY_ACTUAL_PAGE_SIZE))} onNext={() => setOffset(offset + STUDY_ACTUAL_PAGE_SIZE)}
          prevDisabled={offset === 0} nextDisabled={offset + STUDY_ACTUAL_PAGE_SIZE >= visible.total}
          summary={`Всего сетапов: ${visible.total}. Страница ${Math.floor(offset / STUDY_ACTUAL_PAGE_SIZE) + 1} из ${Math.max(1, Math.ceil(visible.total / STUDY_ACTUAL_PAGE_SIZE))}`}
          label="Страницы сетапов после исследования" prevLabel="Предыдущая страница сетапов" nextLabel="Следующая страница сетапов" />
      </>}
    </DialogContent>
  </Dialog>;
}

export function StudyActualSection({ studyId, revisionTrigger }: { studyId: number; revisionTrigger: number }) {
  useSyncExternalStore(subscribeInterfaceSettings, interfaceSettings);
  const detail = useStudyActualDetail(studyId, revisionTrigger);
  const [selection, setSelection] = useState<{ revision: number; jobId: string } | null>(null);
  const section = useRef<HTMLElement>(null); const id = useId();
  const actual = detail.actual;
  const published = actual?.status === "completed" && actual.jobId !== null && actual.counts !== null ? actual : null;
  const report = detail.report;
  const objective = report?.originalObjective;
  const label = objective ? objective.trendMode === "mean" ? "Среднее за исходный период" : `${objective.trendMode === "blend" ? `Смесь · λ ${objective.trendBlendLambda} · ` : ""}${forecastRecipeLabel(objective.forecast ?? undefined, objective.trendBendDays)}` : "Исходный objective недоступен";
  return <section ref={section} tabIndex={-1} aria-label="Матожидание после исследования" className="grid min-w-0 gap-3">
    {detail.error && <Alert variant="destructive" role="alert">{detail.error}<Button type="button" variant="outline" onClick={detail.refresh}>Повторить загрузку отчёта</Button></Alert>}
    {detail.loading && !report && !detail.error && <Spinner label="Загрузка сохранённого расчёта после исследования" />}
    {actual && !published && <p role={actual.status === "failed" ? "alert" : "status"} className={actual.status === "failed" ? "text-sm text-danger" : "text-sm text-muted-foreground"}>
      {isStudyActualPending(actual) ? "Выполняется расчёт после исследования. Предыдущие производные результаты удалены."
        : actual.status === "failed" ? `${actual.error?.code ?? "study_actual_failed"}: ${actual.error?.message ?? "Расчёт завершился ошибкой; повторный запуск выполняется вручную."}`
          : actual.eligibility.eligible ? "Расчёт после исследования ещё не выполнялся. Запустите его вручную в таблице исследований." : `Расчёт после исследования недоступен: ${actual.eligibility.reason ?? "нет подтверждённых исходных данных"}.`}
    </p>}
    {actual && !report && !detail.loading && <p className="text-sm text-muted-foreground">Исходный график недоступен: {actual.originalReportUnavailableReason ?? "сохранённый исходный отчёт отсутствует"}. Наличие исторического запуска не подтверждает сохранённые исходы сделок.</p>}
    {published && <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex flex-wrap items-center gap-2 text-sm"><span>Матожидание факт:</span><span className={`font-medium ${outcomeMeanClass(published.meanR)}`}>{formatOutcomeRisk(published.meanR)}</span>{published.counts && <Badge className="bg-ambient-violet/15 text-ambient-violet" title="Закрытые сделки / все сетапы">{published.counts.closed}/{published.counts.total}</Badge>}{published.meanR === null && <span className="text-muted-foreground">Нет закрытых сделок</span>}<span aria-label="Последний включённый сетап" title="Последний включённый сетап" className="basis-full whitespace-nowrap text-xs text-muted-foreground">{published.lastSetupAt == null ? "—" : formatIsoDateTime(published.lastSetupAt)}</span></div>
        <Button id={id} type="button" variant="outline" className="ml-auto h-auto w-fit max-w-full whitespace-normal text-left" onClick={() => setSelection({ revision: published.revision, jobId: published.jobId! })}>Посмотреть список сетапов после исследования</Button>
      </div>}
    {report && <>
      <StudyActualChart report={report} {...(detail.error ? { combinedUnavailableReason: "post_study_stale" }
        : actual?.status !== "completed" ? { combinedUnavailableReason: `post_study_${actual?.status ?? "unavailable"}` } : {})} />
      {report.display?.historical.availability.status === "unavailable" ? <p className="text-sm text-muted-foreground">Исходная история графика недоступна: {report.display.historical.availability.reason}. Наличие исторического запуска не подтверждает сохранённые исходы сделок.</p>
        : report.historical.availability.status === "unavailable" && <p className="text-sm text-muted-foreground">Исходная история графика недоступна: {report.historical.availability.reason}.</p>}
      {report.display && <p className="text-xs text-muted-foreground">Дневная группировка: {report.display.timeZone}. Исходная история: {report.display.historical.fidelity}{report.display.historical.complete ? "" : " · неполное покрытие"}{report.display.historical.reason ? ` · ${report.display.historical.reason}` : ""}. После исследования: {report.display.actual.fidelity}{report.display.actual.complete ? "" : " · неполное покрытие"}{report.display.actual.reason ? ` · ${report.display.actual.reason}` : ""}.</p>}
      {report.display?.historical.rawReplayAvailable !== undefined && <p className="text-xs text-muted-foreground">Исходный исторический запуск: {report.display.historical.rawReplayAvailable ? "доступен" : "недоступен"}.</p>}
      {report.projection.availability.status === "unavailable" && <p className="text-sm text-muted-foreground">Исходная модельная линия недоступна: {report.projection.availability.reason}. Новый прогноз не подставляется.</p>}
      <p className="text-xs text-muted-foreground">Жёлтая линия: {label}.</p>
      <Separator />
    </>}
    {published && selection?.revision === published.revision && selection.jobId === published.jobId && <ActualSetupsDialog key={`${published.jobId}:${published.revision}`} actual={published} onClose={() => setSelection(null)} onInvalidated={detail.refresh} returnFocus={() => (document.getElementById(id) ?? section.current)?.focus()} />}
  </section>;
}
