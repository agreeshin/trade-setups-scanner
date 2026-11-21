import { useContext, useEffect, useId, useRef, useState, type ReactNode } from "react";
import { Alert } from "../components/ui/alert";
import { Button } from "../components/ui/button";
import { Badge } from "../components/ui/badge";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "../components/ui/dialog";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow, TableSkeletonRows } from "../components/ui/table";
import { formatUnix } from "../lib/datetime-format";
import { formatElapsedDuration } from "../lib/format-duration";
import { isActiveStudy, isContinuousStudy, listOptimizationStudies, type OptimizationStudyAction, type OptimizationStudyPage, type OptimizationStudySummary } from "../lib/optimization-api";
import { isStudyActualPending } from "../lib/study-actual-api";
import { StudyActualFact, StudyActualRefreshButton } from "./study-actual-controls";
import { useStudyActualAction } from "./use-study-actual-action";
import { errorText } from "./platform-controls";
import { RouteActiveContext } from "./route-active";
import type { OptimizationController } from "./use-optimization-studies";

const scoreFormatter = new Intl.NumberFormat("en-US", {
  notation: "standard", useGrouping: false, minimumFractionDigits: 2, maximumFractionDigits: 2,
});

export function OptimizationTrashIcon() {
  return <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="h-4 w-4" aria-hidden="true"><path d="M3 6h18M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2M10 11v6M14 11v6" /></svg>;
}

function OptimizationHistoryRow({ study, latest, controller, busy, instanceId, onSelect, onDelete, onActualChanged }: {
  study: OptimizationStudySummary; latest: OptimizationStudySummary | undefined; controller: OptimizationController; busy: boolean; instanceId: string;
  onSelect: (id: number, triggerId: string, continuous: boolean) => void; onDelete: (study: OptimizationStudyAction) => void; onActualChanged: () => void;
}) {
  const actual = useStudyActualAction(study.id, study.actual, onActualChanged);
  return <TableRow>
    <TableCell><Button id={`${instanceId}-study-${study.id}`} type="button" variant="outline" size="sm" aria-label={`Результат исследования ${study.id}`} onClick={() => onSelect(study.id, `${instanceId}-study-${study.id}`, isContinuousStudy(study))}>#{study.id}</Button></TableCell>
    <TableCell><Badge>{isContinuousStudy(study) ? "Непрерывное" : "Ручное"}</Badge></TableCell>
    <TableCell className="whitespace-nowrap">[{formatUnix(study.spec.fromUnix)}, {formatUnix(study.spec.toUnix)})</TableCell>
    <TableCell className="whitespace-nowrap">{formatElapsedDuration(study.progress.elapsedSeconds)}</TableCell>
    <TableCell>{study.spec.minSetupsPerDay ?? "Гейт не применялся"}</TableCell>
    <TableCell>{study.bestScore === null || !Number.isFinite(study.bestScore) ? "—" : `${scoreFormatter.format(study.bestScore)} R`}</TableCell>
    <TableCell>{study.forecastBest == null || !Number.isFinite(study.forecastBest.forecastR) ? "—" : `${scoreFormatter.format(study.forecastBest.forecastR)} R`}</TableCell>
    <TableCell><div className="max-h-14 overflow-y-auto"><StudyActualFact action={actual} /></div></TableCell>
    <TableCell><div className="flex items-center gap-1">
      <StudyActualRefreshButton studyId={study.id} action={actual} />
      {isActiveStudy(study)
        ? isContinuousStudy(study) ? <span className="text-xs text-muted-foreground">Общая остановка в настройках алгоритма</span> : <Button type="button" variant="outline" size="sm" disabled={busy || study.status === "cancel_requested" || (latest?.id === study.id && latest.status === "cancel_requested")} aria-label={`Остановить исследование ${study.id}`} onClick={() => { void controller.stop(study); }}>{study.status === "cancel_requested" || (latest?.id === study.id && latest.status === "cancel_requested") ? "Остановка запрошена" : controller.cancelling === study.id ? "Запрашиваем остановку…" : "Остановить"}</Button>
        : <Button type="button" variant="outline" size="sm" disabled={busy} aria-label={`Удалить исследование ${study.id}`} onClick={() => onDelete(study)}><OptimizationTrashIcon /></Button>}
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
  const routeActive = useContext(RouteActiveContext);
  const [offset, setOffset] = useState(0);
  const [page, setPage] = useState<OptimizationStudyPage | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [retry, setRetry] = useState(0);
  const latest = controller.latestByAlgorithm[algorithmId] ?? controller.byAlgorithm[algorithmId];
  // Active first-page rows follow compact discovery polling; terminal pages do not
  // refetch just because unrelated polling produced a new object identity.
  const observed = offset === 0 && latest && isActiveStudy(latest) ? latest : latest?.updatedAt;
  const latestId = latest?.id;
  const latestStatus = latest?.status;
  const busy = controller.creating || controller.cancelling !== null || controller.deleting !== null;
  useEffect(() => {
    if (!active || !routeActive || !controller.ready) return;
    let disposed = false;
    let request: AbortController | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let watchActual = false;
    async function load() {
      clearTimeout(timer);
      request?.abort();
      if (document.visibilityState === "hidden") return;
      const current = new AbortController(); request = current;
      setLoading(true); setError(null);
      // Keep same-page buttons mounted during refresh so close-focus restoration
      // and polling cannot lose the originating ID. Never keep another page.
      setPage((previous) => previous?.offset === offset && previous.limit === pageSize ? previous : null);
      try {
        const next = await listOptimizationStudies(algorithmId, current.signal, pageSize, offset);
        if (disposed || current.signal.aborted) return;
        if (offset > 0 && offset >= next.total) {
          setOffset(next.total === 0 ? 0 : Math.floor((next.total - 1) / pageSize) * pageSize);
          return;
        }
        setPage(next);
        watchActual = next.studies.some((study) => isStudyActualPending(study.actual));
      } catch (cause) {
        if (!disposed && !current.signal.aborted) setError(`История оптимизации недоступна: ${errorText(cause)}`);
      } finally {
        if (!disposed && !current.signal.aborted) {
          setLoading(false);
          if (watchActual) timer = setTimeout(() => { void load(); }, 3000);
        }
      }
    }
    const visibility = () => { clearTimeout(timer); request?.abort(); if (document.visibilityState !== "hidden") void load(); };
    void load(); document.addEventListener("visibilitychange", visibility);
    return () => { disposed = true; clearTimeout(timer); request?.abort(); document.removeEventListener("visibilitychange", visibility); };
  }, [algorithmId, offset, controller.ready, controller.revision, observed, latestId, latestStatus, retry, routeActive, active, pageSize]);
  const currentPage = page?.offset === offset && page.limit === pageSize ? page : null;
  const placeholders = currentPage === null && (loading || page !== null);
  const padding = placeholders ? 0 : Math.max(0, pageSize - (currentPage?.studies.length ?? 0));
  return <section id={id} tabIndex={-1} aria-label={`История оптимизации · ${algorithmName}`} className="grid min-w-0 gap-3">
    {error && <Alert variant="destructive" role="alert">{error}<Button type="button" variant="outline" onClick={() => setRetry((value) => value + 1)}>Повторить загрузку истории</Button></Alert>}
    {actions}
    <div className="overflow-x-auto">
      <Table aria-label={`Исследования · ${algorithmName}`} aria-busy={loading || placeholders} className="table-fixed min-w-7xl">
        <colgroup>{["w-20", "w-28", "w-96", "w-48", "w-40", "w-32", "w-44", "w-52", "w-60"].map((width, index) => <col key={index} className={width} />)}</colgroup>
        <TableHeader><TableRow>{["ID", "Происхождение", "Диапазон дат", "Время исследования", "Минимум сетапов в день", "Лучший Score", "Лучший прогноз матожидания", "Матожидание факт", "Действия"].map((label) => <TableHead key={label}>{label}</TableHead>)}</TableRow></TableHeader>
        <TableBody className="[&_tr]:h-20">{placeholders ? <TableSkeletonRows rows={pageSize} columns={9} /> : <>
          {currentPage?.studies.map((study) => <OptimizationHistoryRow key={study.id} study={study} latest={latest} controller={controller} busy={busy} instanceId={instanceId} onSelect={onSelect} onDelete={onDelete} onActualChanged={() => setRetry((value) => value + 1)} />)}
          {Array.from({ length: padding }, (_, index) => <TableRow key={`padding-${index}`} aria-hidden="true" className="hover:bg-transparent"><TableCell colSpan={9} /></TableRow>)}
        </>}</TableBody>
      </Table>
    </div>
    {pagination && <div className="flex min-h-8 flex-wrap items-center gap-2 text-sm" aria-label={`Страницы исследований · ${algorithmName}`}>
      <Button type="button" variant="outline" size="sm" disabled={loading || placeholders || currentPage === null || offset === 0} aria-label={`Предыдущая страница исследований · ${algorithmName}`} onClick={() => setOffset(Math.max(0, offset - pageSize))}>Назад</Button>
      <span>Страница {Math.floor(offset / pageSize) + 1} · всего {page?.total ?? "—"}</span>
      <Button type="button" variant="outline" size="sm" disabled={loading || placeholders || currentPage === null || offset + pageSize >= currentPage.total} aria-label={`Следующая страница исследований · ${algorithmName}`} onClick={() => setOffset(offset + pageSize)}>Далее</Button>
    </div>}
    {currentPage?.total === 0 && <p className="text-sm text-muted-foreground">Исследований пока нет.</p>}
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
