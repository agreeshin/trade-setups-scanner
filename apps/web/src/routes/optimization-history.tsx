import { useContext, useEffect, useId, useLayoutEffect, useRef, useState, useSyncExternalStore, type ReactNode } from "react";
import { Alert } from "../components/ui/alert";
import { Button } from "../components/ui/button";
import { Badge } from "../components/ui/badge";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "../components/ui/dialog";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow, TableSkeletonRows } from "../components/ui/table";
import { formatUnix, interfaceSettings, subscribeInterfaceSettings } from "../lib/datetime-format";
import { formatElapsedDuration } from "../lib/format-duration";
import { forecastRecipeLabel } from "../lib/forecast-recipe";
import { canDeleteStudy, isActiveStudy, isContinuousStudy, isStudyTransitioning, listOptimizationStudies, type OptimizationStudyAction, type OptimizationStudyPage, type OptimizationStudySummary } from "../lib/optimization-api";
import { isStudyActualPending } from "../lib/study-actual-api";
import { StudyActualFact, StudyActualRefreshButton } from "./study-actual-controls";
import { StudyActualContinuousControls } from "./study-actual-continuous";
import { useStudyActualAction } from "./use-study-actual-action";
import { errorText } from "./platform-controls";
import { RouteActiveContext } from "./route-active";
import type { OptimizationController } from "./use-optimization-studies";
import { ManualStudyActions, StudyStatusBadge } from "./optimization-results";
import { StudyPresentationDialog } from "./study-presentation-dialog";

const scoreFormatter = new Intl.NumberFormat("en-US", {
  notation: "standard", useGrouping: false, minimumFractionDigits: 2, maximumFractionDigits: 2,
});
const headers = [
  ["ID"], ["Происхождение"], ["Диапазон", "дат"], ["Время", "исследования"], ["Минимум сетапов", "в день"],
  ["Оптимизация", "матожидания"], ["Лучшая ячейка", "TP/SL"], ["Лучший Score", "матрицы"],
  ["Лучший прогноз", "для ячейки"], ["Матожидание", "факт"], ["График PNG"], ["Действия"],
];

export function OptimizationTrashIcon() {
  return <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="h-4 w-4" aria-hidden="true"><path d="M3 6h18M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2M10 11v6M14 11v6" /></svg>;
}

function OptimizationExpectationSummary({ spec }: { spec: OptimizationStudySummary["spec"] }) {
  const mode = spec.trendMode ?? "mean";
  if (mode === "mean") return <>Среднее</>;
  const lambda = spec.trendBlendLambda ?? 0.7;
  const bend = spec.forecast?.method === "trend" ? spec.forecast.bendDays : spec.trendBendDays ?? "linear";
  const method = spec.forecast?.method === "decayed_probability"
    ? `Вероятность TP с памятью · H ${spec.forecast.halfLifeDays} дней`
    : `Тренд · ${bend === "linear" ? "прямая" : `перегиб ${bend} дней`}`;
  return <div className="grid max-w-48 gap-1 break-words text-xs">
    <span>{mode === "blend" ? "Смесь" : "Прогноз"} · λ {Number.isFinite(lambda) ? lambda : "—"}{mode === "forecast" ? " (не применяется)" : ""}</span>
    <span title={forecastRecipeLabel(spec.forecast, bend)}>{method}</span>
  </div>;
}

function OptimizationHistoryRow({ study, latest, controller, busy, instanceId, onSelect, onDelete, onActualChanged, onPresentation }: {
  study: OptimizationStudySummary; latest: OptimizationStudySummary | undefined; controller: OptimizationController; busy: boolean; instanceId: string;
  onSelect: (id: number, triggerId: string, continuous: boolean) => void; onDelete: (study: OptimizationStudyAction) => void; onActualChanged: () => void;
  onPresentation: (id: number, triggerId: string) => void;
}) {
  const actual = useStudyActualAction(study.id, study.actual, onActualChanged);
  const actionStudy = latest?.id === study.id && isActiveStudy(study)
    && ((latest.manualControl?.revision ?? -1) > (study.manualControl?.revision ?? -1) || latest.status === "cancel_requested") ? latest : study;
  const activeContinuous = isActiveStudy(study) && isContinuousStudy(study);
  const deleteReasonId = `${instanceId}-study-${study.id}-delete-reason`;
  const presentationId = `${instanceId}-study-${study.id}-png`;
  const presentation = study.currentPresentation;
  const unavailableReason = presentation?.status === "unavailable" ? presentation.reason : "сохранённый снимок отсутствует";
  return <TableRow>
    <TableCell><div className="grid justify-items-start gap-1"><Button id={`${instanceId}-study-${study.id}`} type="button" variant="outline" size="sm" aria-label={`Результат исследования ${study.id}`} onClick={() => onSelect(study.id, `${instanceId}-study-${study.id}`, isContinuousStudy(study))}>#{study.id}</Button><StudyStatusBadge study={actionStudy} /></div></TableCell>
    <TableCell><Badge>{isContinuousStudy(study) ? "Непрерывное" : "Ручное"}</Badge></TableCell>
    <TableCell className="whitespace-nowrap text-xs" title="Начало включительно; конец исключительно."><span className="block">{formatUnix(study.spec.fromUnix)} -</span><span className="block">{formatUnix(study.spec.toUnix)}</span></TableCell>
    <TableCell className="whitespace-nowrap">{formatElapsedDuration(study.progress.elapsedSeconds)}</TableCell>
    <TableCell>{study.spec.minSetupsPerDay ?? "Гейт не применялся"}</TableCell>
    <TableCell><OptimizationExpectationSummary spec={study.spec} /></TableCell>
    <TableCell className="whitespace-nowrap">{study.forecastBest == null || !Number.isFinite(study.forecastBest.tpPct) || !Number.isFinite(study.forecastBest.slPct)
      ? "—" : `TP ${study.forecastBest.tpPct}% / SL ${study.forecastBest.slPct}%`}</TableCell>
    <TableCell>{study.bestScore === null || !Number.isFinite(study.bestScore) ? "—" : `${scoreFormatter.format(study.bestScore)} R`}</TableCell>
    <TableCell>{study.forecastBest == null || !Number.isFinite(study.forecastBest.forecastR) ? "—" : `${scoreFormatter.format(study.forecastBest.forecastR)} R`}</TableCell>
    <TableCell><StudyActualFact action={actual} /></TableCell>
    <TableCell className="w-px whitespace-nowrap">{presentation?.status === "available"
      ? <Button id={presentationId} type="button" variant="outline" size="sm" aria-label={`График PNG исследования ${study.id}`} onClick={() => onPresentation(study.id, presentationId)}>Открыть PNG</Button>
      : <><Button type="button" variant="outline" size="sm" disabled aria-label={`График PNG исследования ${study.id} недоступен`} aria-describedby={`${presentationId}-reason`} title={unavailableReason}>Недоступен</Button><span id={`${presentationId}-reason`} className="sr-only">{unavailableReason}</span></>}</TableCell>
    <TableCell className="w-px whitespace-nowrap pr-0"><div className="flex items-center gap-1">
      <StudyActualRefreshButton studyId={study.id} action={actual} />
      <ManualStudyActions study={actionStudy} controller={controller} compact />
      <Button type="button" variant="outline" size="sm" disabled={busy || !canDeleteStudy(actionStudy)} aria-label={`Удалить исследование ${study.id}`} aria-describedby={activeContinuous ? deleteReasonId : undefined} onClick={() => onDelete(actionStudy)}><OptimizationTrashIcon /></Button>
      {activeContinuous && <span id={deleteReasonId} className="sr-only">Удаление недоступно, пока непрерывное исследование выполняется.</span>}
    </div></TableCell>
  </TableRow>;
}

/** One replaceable page of compact summaries per card; no full detail or page archive. */
export function OptimizationHistory({ id, algorithmId, algorithmName, controller, onSelect, onDelete, pageSize = 20, pagination = true, active = true, actions }: {
  id?: string;
  algorithmId: string; algorithmName: string; controller: OptimizationController;
  onSelect: (id: number, triggerId: string, continuous: boolean) => void; onDelete: (study: OptimizationStudyAction) => void;
  pageSize?: number; pagination?: boolean; active?: boolean; actions?: ReactNode;
}) {
  const instanceId = useId();
  const regionId = id ?? `${instanceId}-history`;
  const [presentationSelection, setPresentationSelection] = useState<{ id: number; triggerId: string; algorithmId: string; offset: number } | null>(null);
  useSyncExternalStore(subscribeInterfaceSettings, interfaceSettings);
  const routeActive = useContext(RouteActiveContext);
  const [offset, setOffset] = useState(0);
  const [page, setPage] = useState<(OptimizationStudyPage & { algorithmId: string }) | null>(null);
  const [fetching, setFetching] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [retry, setRetry] = useState(0);
  const refreshPage = useRef<() => void>(() => undefined);
  const mutationRevision = useRef(controller.revision);
  useLayoutEffect(() => { mutationRevision.current = controller.revision; }, [controller.revision]);
  const latest = controller.latestByAlgorithm[algorithmId] ?? controller.byAlgorithm[algorithmId];
  // Preserve first-page progress invalidation; later pages observe their own rows.
  // Discovery refreshes coalesce without discarding useful in-flight reads.
  const observed = offset === 0 && latest && isActiveStudy(latest) ? latest : latest?.updatedAt;
  const latestId = latest?.id;
  const latestStatus = latest?.status;
  const busy = controller.creating || controller.cancelling !== null || controller.deleting !== null || controller.controlling != null;
  useEffect(() => {
    if (!active || !routeActive || !controller.ready) return;
    let disposed = false;
    let request: AbortController | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let watchActual = false;
    let watchStudies = false;
    let fastStudies = false;
    let inFlight = false;
    let refreshQueued = false;
    async function load() {
      if (disposed || document.visibilityState === "hidden") return;
      clearTimeout(timer);
      // Discovery and row actions invalidate this page, but do not own its read.
      // Coalesce updates into one follow-up rather than aborting useful work.
      if (inFlight) { refreshQueued = true; return; }
      inFlight = true;
      const current = new AbortController(); request = current;
      const generation = mutationRevision.current;
      setFetching(true); setError(null);
      // Keep same-page buttons mounted during refresh so close-focus restoration
      // and polling cannot lose the originating ID. Never keep another page.
      setPage((previous) => previous?.algorithmId === algorithmId && previous.offset === offset && previous.limit === pageSize ? previous : null);
      try {
        const next = await listOptimizationStudies(algorithmId, current.signal, pageSize, offset);
        if (disposed || current.signal.aborted || generation !== mutationRevision.current) return;
        if (offset > 0 && offset >= next.total) {
          refreshQueued = false; watchActual = false;
          setOffset(next.total === 0 ? 0 : Math.floor((next.total - 1) / pageSize) * pageSize);
          return;
        }
        setPage({ ...next, algorithmId });
        watchActual = next.studies.some((study) => isStudyActualPending(study.actual));
        watchStudies = next.studies.some(isActiveStudy);
        fastStudies = next.studies.some(isStudyTransitioning);
      } catch (cause) {
        if (!disposed && !current.signal.aborted && generation === mutationRevision.current) setError(`История оптимизации недоступна: ${errorText(cause)}`);
      } finally {
        if (!disposed && !current.signal.aborted) {
          inFlight = false;
          if (refreshQueued) { refreshQueued = false; void load(); }
          else {
            setFetching(false);
            if (watchActual || watchStudies) timer = setTimeout(() => { void load(); }, watchActual || fastStudies ? 3000 : 15000);
          }
        }
      }
    }
    const refresh = () => { void load(); };
    refreshPage.current = refresh;
    const visibility = () => {
      clearTimeout(timer); request?.abort(); inFlight = false; refreshQueued = false;
      if (document.visibilityState !== "hidden") refresh();
    };
    document.addEventListener("visibilitychange", visibility);
    return () => {
      disposed = true; clearTimeout(timer); request?.abort();
      document.removeEventListener("visibilitychange", visibility);
      if (refreshPage.current === refresh) refreshPage.current = () => undefined;
    };
  }, [algorithmId, offset, controller.ready, routeActive, active, pageSize]);
  // Refresh triggers share the stable displayed-page owner above. Only a scope
  // change or deactivation tears it down and fences its late responses.
  useEffect(() => {
    refreshPage.current();
  }, [algorithmId, offset, controller.ready, controller.revision, observed, latestId, latestStatus, retry, routeActive, active, pageSize]);
  const currentPage = page?.algorithmId === algorithmId && page.offset === offset && page.limit === pageSize ? page : null;
  // A missing requested page blocks interaction; refreshing valid rows does not.
  const placeholders = currentPage === null && (fetching || page !== null);
  const padding = placeholders ? 0 : Math.max(0, pageSize - (currentPage?.studies.length ?? 0));
  const presentationStudy = presentationSelection?.algorithmId === algorithmId && presentationSelection.offset === offset
    ? currentPage?.studies.find((study) => study.id === presentationSelection.id) : undefined;
  return <section id={regionId} tabIndex={-1} aria-label={`История оптимизации · ${algorithmName}`} className="grid min-w-0 gap-3">
    {error && <Alert variant="destructive" role="alert">{error}<Button type="button" variant="outline" onClick={() => setRetry((value) => value + 1)}>Повторить загрузку истории</Button></Alert>}
    {actions && <div className="flex min-w-0 flex-wrap items-start justify-between gap-3">{actions}<StudyActualContinuousControls algorithmId={algorithmId} active={active && controller.ready} onChanged={() => refreshPage.current()} /></div>}
    {/* Reserve 20 h-13 rows plus a header outside the table; real rows never inherit its height. */}
    <div className={pageSize === 20 ? "min-h-[68.25rem] overflow-x-auto" : "overflow-x-auto"}>
      <Table aria-label={`Исследования · ${algorithmName}`} aria-busy={fetching || placeholders} className="table-auto">
        <TableHeader><TableRow>{headers.map((lines, index) => <TableHead key={index} aria-label={lines.join(" ")} className={index === headers.length - 1 ? "w-px px-2 pr-0" : "px-2"}>{lines.map((line) => <span key={line} className="block whitespace-nowrap">{line}</span>)}</TableHead>)}</TableRow></TableHeader>
        <TableBody className="[&_td]:px-2 [&_td]:py-1.5 [&_td]:align-middle [&_td:last-child]:pr-0">{placeholders ? <TableSkeletonRows rows={pageSize} columns={headers.length} /> : <>
          {currentPage?.studies.map((study) => <OptimizationHistoryRow key={study.id} study={study} latest={latest} controller={controller} busy={busy} instanceId={instanceId} onSelect={onSelect} onDelete={onDelete} onActualChanged={() => setRetry((value) => value + 1)} onPresentation={(studyId, triggerId) => setPresentationSelection({ id: studyId, triggerId, algorithmId, offset })} />)}
          {Array.from({ length: padding }, (_, index) => <TableRow key={`padding-${index}`} aria-hidden="true" className="h-13 hover:bg-transparent"><TableCell colSpan={headers.length} /></TableRow>)}
        </>}</TableBody>
      </Table>
    </div>
    {pagination && <div className="flex min-h-8 flex-wrap items-center gap-2 text-sm" aria-label={`Страницы исследований · ${algorithmName}`}>
      <Button type="button" variant="outline" size="sm" disabled={placeholders || currentPage === null || offset === 0} aria-label={`Предыдущая страница исследований · ${algorithmName}`} onClick={() => { setPresentationSelection(null); setOffset(Math.max(0, offset - pageSize)); }}>Назад</Button>
      <span>Страница {Math.floor(offset / pageSize) + 1} · всего {page?.total ?? "—"}</span>
      <Button type="button" variant="outline" size="sm" disabled={placeholders || currentPage === null || offset + pageSize >= currentPage.total} aria-label={`Следующая страница исследований · ${algorithmName}`} onClick={() => { setPresentationSelection(null); setOffset(offset + pageSize); }}>Далее</Button>
    </div>}
    {currentPage?.total === 0 && <p className="text-sm text-muted-foreground">Исследований пока нет.</p>}
    {active && routeActive && presentationSelection && presentationStudy?.currentPresentation?.status === "available" && <StudyPresentationDialog studyId={presentationStudy.id} snapshot={presentationStudy.currentPresentation.snapshot} returnFocusId={presentationSelection.triggerId} fallbackFocusId={regionId} onClose={() => setPresentationSelection(null)} onRefresh={() => refreshPage.current()} />}
  </section>;
}

export function OptimizationDeleteDialog({ target, controller, onClose, onDeleted, fallbackFocusId }: {
  target: OptimizationStudyAction | null; controller: OptimizationController;
  onClose: () => void; onDeleted: (id: number) => void; fallbackFocusId?: string;
}) {
  const deleting = controller.deleting !== null;
  const removed = useRef(false);
  return <Dialog open={target !== null} onOpenChange={(open) => { if (!open && !deleting) onClose(); }}>
    {target && <DialogContent onCloseAutoFocus={(event) => {
      if (removed.current && fallbackFocusId !== undefined) { event.preventDefault(); document.getElementById(fallbackFocusId)?.focus(); }
      removed.current = false;
    }}>
      <DialogHeader>
        <DialogTitle>Удалить исследование #{target.id}?</DialogTitle>
        <DialogDescription>Исследование и сохранённый результат будут удалены без возможности восстановления. Обычные прогоны, кеши, настройки и рыночные данные не изменятся.</DialogDescription>
      </DialogHeader>
      {controller.error && <Alert variant="destructive" role="alert">{controller.error}</Alert>}
      <div className="flex justify-end gap-2">
        <Button type="button" variant="outline" disabled={deleting} onClick={onClose}>Отмена</Button>
        <Button type="button" disabled={deleting} onClick={() => { void controller.remove(target).then((ok) => { if (ok) { removed.current = true; onDeleted(target.id); } }); }}>{deleting ? "Удаление…" : "Удалить"}</Button>
      </div>
    </DialogContent>}
  </Dialog>;
}
