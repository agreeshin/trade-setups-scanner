import { useContext, useEffect, useId, useState } from "react";
import { Alert } from "../components/ui/alert";
import { Button } from "../components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "../components/ui/dialog";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "../components/ui/table";
import { formatUnix } from "../lib/datetime-format";
import { formatElapsedDuration } from "../lib/format-duration";
import { isActiveStudy, listOptimizationStudies, type OptimizationStudyAction, type OptimizationStudyPage } from "../lib/optimization-api";
import { errorText } from "./platform-controls";
import { RouteActiveContext } from "./route-active";
import type { OptimizationController } from "./use-optimization-studies";

const scoreFormatter = new Intl.NumberFormat("en-US", {
  notation: "standard", useGrouping: false, minimumFractionDigits: 2, maximumFractionDigits: 2,
});

export function OptimizationTrashIcon() {
  return <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="h-4 w-4" aria-hidden="true"><path d="M3 6h18M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2M10 11v6M14 11v6" /></svg>;
}

/** One replaceable page of compact summaries per card; no full detail or page archive. */
export function OptimizationHistory({ algorithmId, algorithmName, controller, onSelect, onDelete, pageSize = 10, pagination = true, active = true }: {
  algorithmId: string; algorithmName: string; controller: OptimizationController;
  onSelect: (id: number, triggerId: string) => void; onDelete: (study: OptimizationStudyAction) => void;
  pageSize?: number; pagination?: boolean; active?: boolean;
}) {
  const instanceId = useId();
  const routeActive = useContext(RouteActiveContext);
  const [offset, setOffset] = useState(0);
  const [page, setPage] = useState<OptimizationStudyPage | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [retry, setRetry] = useState(0);
  const latest = controller.byAlgorithm[algorithmId];
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
    async function load() {
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
      } catch (cause) {
        if (!disposed && !current.signal.aborted) setError(`История оптимизации недоступна: ${errorText(cause)}`);
      } finally {
        if (!disposed && !current.signal.aborted) setLoading(false);
      }
    }
    const visibility = () => { request?.abort(); if (document.visibilityState !== "hidden") void load(); };
    void load(); document.addEventListener("visibilitychange", visibility);
    return () => { disposed = true; request?.abort(); document.removeEventListener("visibilitychange", visibility); };
  }, [algorithmId, offset, controller.ready, controller.revision, observed, latestId, latestStatus, retry, routeActive, active, pageSize]);
  return <section aria-label={`История оптимизации · ${algorithmName}`} className="grid min-w-0 gap-3">
    <h5 className="font-semibold">{pagination ? "История оптимизации" : "Последние исследования"}</h5>
    {error && <Alert variant="destructive" role="alert">{error}<Button type="button" variant="outline" onClick={() => setRetry((value) => value + 1)}>Повторить загрузку истории</Button></Alert>}
    <div className="overflow-x-auto">
      <Table aria-label={`Исследования · ${algorithmName}`} aria-busy={loading}>
        <TableHeader><TableRow>{["ID", "Диапазон дат", "Время исследования", "Минимум сетапов в день", "Лучшее среднее матожидание", "Действия"].map((label) => <TableHead key={label}>{label}</TableHead>)}</TableRow></TableHeader>
        <TableBody>{page?.studies.map((study) => <TableRow key={study.id}>
          <TableCell><Button id={`${instanceId}-study-${study.id}`} type="button" variant="outline" size="sm" aria-label={`Результат исследования ${study.id}`} onClick={() => onSelect(study.id, `${instanceId}-study-${study.id}`)}>#{study.id}</Button></TableCell>
          <TableCell className="whitespace-nowrap">[{formatUnix(study.spec.fromUnix)}, {formatUnix(study.spec.toUnix)})</TableCell>
          <TableCell className="whitespace-nowrap">{formatElapsedDuration(study.progress.elapsedSeconds)}</TableCell>
          <TableCell>{study.spec.minSetupsPerDay ?? "Гейт не применялся"}</TableCell>
          <TableCell>{study.bestScore === null || !Number.isFinite(study.bestScore) ? "—" : `${scoreFormatter.format(study.bestScore)} R`}</TableCell>
          <TableCell>{isActiveStudy(study)
            ? <Button type="button" variant="outline" size="sm" disabled={busy || study.status === "cancel_requested" || (latest?.id === study.id && latest.status === "cancel_requested")} aria-label={`Остановить исследование ${study.id}`} onClick={() => { void controller.stop(study); }}>{study.status === "cancel_requested" || (latest?.id === study.id && latest.status === "cancel_requested") ? "Остановка запрошена" : controller.cancelling === study.id ? "Запрашиваем остановку…" : "Остановить"}</Button>
            : <Button type="button" variant="outline" size="sm" disabled={busy} aria-label={`Удалить исследование ${study.id}`} onClick={() => onDelete(study)}><OptimizationTrashIcon /></Button>}</TableCell>
        </TableRow>)}</TableBody>
      </Table>
    </div>
    {loading && <p className="text-sm text-muted-foreground" role="status">Загрузка истории исследований…</p>}
    {pagination && page && <div className="flex flex-wrap items-center gap-2 text-sm">
      <Button type="button" variant="outline" size="sm" disabled={loading || page.offset === 0} aria-label={`Предыдущая страница исследований · ${algorithmName}`} onClick={() => setOffset(Math.max(0, offset - pageSize))}>Назад</Button>
      <span>Страница {Math.floor(page.offset / pageSize) + 1} · всего {page.total}</span>
      <Button type="button" variant="outline" size="sm" disabled={loading || page.offset + pageSize >= page.total} aria-label={`Следующая страница исследований · ${algorithmName}`} onClick={() => setOffset(offset + pageSize)}>Далее</Button>
    </div>}
    {page?.total === 0 && <p className="text-sm text-muted-foreground">Исследований пока нет.</p>}
  </section>;
}

export function OptimizationDeleteDialog({ target, controller, onClose, onDeleted }: {
  target: OptimizationStudyAction | null; controller: OptimizationController;
  onClose: () => void; onDeleted: (id: number) => void;
}) {
  const deleting = controller.deleting !== null;
  return <Dialog open={target !== null} onOpenChange={(open) => { if (!open && !deleting) onClose(); }}>
    {target && <DialogContent>
      <DialogHeader>
        <DialogTitle>Удалить исследование #{target.id}?</DialogTitle>
        <DialogDescription>Исследование и сохранённый результат будут удалены без возможности восстановления. Обычные прогоны, кеши, настройки и рыночные данные не изменятся.</DialogDescription>
      </DialogHeader>
      {controller.error && <Alert variant="destructive" role="alert">{controller.error}</Alert>}
      <div className="flex justify-end gap-2">
        <Button type="button" variant="outline" disabled={deleting} onClick={onClose}>Отмена</Button>
        <Button type="button" disabled={deleting} onClick={() => { void controller.remove(target).then((removed) => { if (removed) onDeleted(target.id); }); }}>{deleting ? "Удаление…" : "Удалить"}</Button>
      </div>
    </DialogContent>}
  </Dialog>;
}
