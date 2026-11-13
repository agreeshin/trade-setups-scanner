import { useEffect, useId, useRef, useState } from "react";
import { Alert } from "../components/ui/alert";
import { Button } from "../components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "../components/ui/dialog";
import { Spinner } from "../components/ui/spinner";
import { ContinuousCleanupApiError, deleteContinuousHistory, previewContinuousCleanup, type ContinuousCleanupCounts, type ContinuousCleanupPreview, type ContinuousCleanupRetained, type ContinuousCleanupResult } from "../lib/continuous-cleanup-api";
import { errorText } from "./platform-controls";

const removedLabels: Record<keyof ContinuousCleanupCounts, string> = {
  studies: "Непрерывные исследования", generations: "Поколения", profiles: "Профильные результаты (сброс)",
  checkpoints: "Сохранённое состояние сканирования", notificationJobs: "Задания адаптивной доставки", processesReset: "Служебное состояние процессов (сброс)",
};
const retainedLabels: Record<keyof ContinuousCleanupRetained, string> = {
  runs: "Исторические прогоны", batches: "Пакетные запуски", qualityIntervals: "Интервалы качества", analyticsResults: "Аналитические результаты",
  liveSetups: "Опубликованные адаптивные сетапы", configuredProcesses: "Конфигурации процессов", configuredSlots: "Настроенные слоты",
};
function Counters<T extends object>({ value, labels }: { value: T; labels: Record<keyof T, string> }) {
  return <ul className="grid gap-1 text-sm">{(Object.keys(labels) as (keyof T)[]).map((key) => <li key={String(key)}>{labels[key]}: {String(value[key])}</li>)}</ul>;
}
function blockerMessage(code: string): string {
  if (code === "continuous_not_stopped") return "Сначала остановите непрерывные процессы всех алгоритмов и дождитесь завершения остановки, прогрева и доставки.";
  if (code === "ownership_conflict") return "Обнаружен конфликт владения связанными данными. Очистка запрещена; требуется проверка на сервере.";
  return "Сервер запретил очистку: состояние пока не поддерживается интерфейсом. Обновите предварительный просмотр.";
}

/** One owner-wide command. The modal owns one replaceable preview and one request; no job lifetime is driven here. */
export function ContinuousHistoryCleanup({ onDeleted, onRefreshStates, disabled = false }: {
  onDeleted: () => void; onRefreshStates: () => void; disabled?: boolean;
}) {
  const triggerId = useId();
  const cancelId = useId();
  const mutation = useRef<AbortController | null>(null);
  const [open, setOpen] = useState(false);
  const [preview, setPreview] = useState<ContinuousCleanupPreview | null>(null);
  const [result, setResult] = useState<ContinuousCleanupResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [retry, setRetry] = useState(0);
  useEffect(() => () => { mutation.current?.abort(); }, []);
  useEffect(() => {
    if (!open) return;
    const request = new AbortController();
    void previewContinuousCleanup(request.signal).then((next) => {
      if (!request.signal.aborted) setPreview(next);
    }).catch((cause: unknown) => {
      if (!request.signal.aborted) setError(errorText(cause));
    }).finally(() => { if (!request.signal.aborted) setLoading(false); });
    return () => request.abort();
  }, [open, retry]);
  function showModal() { setPreview(null); setLoading(true); setError(null); setResult(null); setOpen(true); }
  function closeModal() { setOpen(false); setPreview(null); }
  function reloadPreview() { setPreview(null); setLoading(true); setError(null); setRetry((value) => value + 1); }
  const hasChanges = preview !== null && Object.values(preview.counts).some((count) => count > 0);
  async function confirm() {
    if (!preview?.eligible || preview.blockers.length || !hasChanges || disabled || mutation.current) return;
    const request = new AbortController(); mutation.current = request;
    setDeleting(true); setError(null);
    try {
      const next = await deleteContinuousHistory(preview.fingerprint, request.signal);
      if (request.signal.aborted) return;
      setResult(next); setOpen(false); setPreview(null); onDeleted();
    } catch (cause) {
      if (request.signal.aborted) return;
      setError(errorText(cause));
      // Only a validated conflict preview can re-enable a fresh explicit confirmation.
      // A lost acknowledgement requires a new read, never an automatic destructive retry.
      setPreview(cause instanceof ContinuousCleanupApiError ? cause.preview ?? null : null);
      onRefreshStates();
    } finally {
      if (mutation.current === request) mutation.current = null;
      if (!request.signal.aborted) setDeleting(false);
    }
  }
  return <section aria-label="Очистка истории непрерывных исследований" className="grid min-w-0 gap-3">
    <div><Button id={triggerId} type="button" variant="outline" disabled={disabled || deleting} onClick={showModal}>Удалить историю непрерывных исследований…</Button></div>
    {result && <div role="status" className="grid gap-2 text-sm"><p>История непрерывных исследований удалена. Исторические запуски, аналитика, live-сетапы, настройки и черновики сохранены.</p><Counters value={result.deleted} labels={removedLabels} /></div>}
    <Dialog open={open} onOpenChange={(next) => { if (!mutation.current && !next) closeModal(); }}>
      <DialogContent className="max-w-2xl" onOpenAutoFocus={(event) => { event.preventDefault(); document.getElementById(cancelId)?.focus(); }} onCloseAutoFocus={(event) => { event.preventDefault(); document.getElementById(triggerId)?.focus(); }}>
        <DialogHeader>
          <DialogTitle>Удалить всю историю непрерывных исследований?</DialogTitle>
          <DialogDescription>Область: все непрерывные исследования всех алгоритмов вашей учётной записи за всё время, а не только видимые строки. Действие необратимо и само не останавливает работу.</DialogDescription>
        </DialogHeader>
        <div className="grid min-h-0 flex-1 gap-3 overflow-y-auto" aria-busy={loading || deleting}>
          <p className="text-sm">Удаляются непрерывные исследования, поколения, сохранённое состояние сканирования, прогрев и задания адаптивной доставки; профильные результаты и служебное состояние процессов сбрасываются. Настроенные процессы и слоты сохраняются.</p>
          <p className="text-sm">Сохраняются ручные исследования, все исторические запуски и результаты, аналитика (включая результаты непрерывных исследований), общие кеши, опубликованные live-сетапы и рекомендации, рыночная история, настройки алгоритмов и общий черновик. Чужие данные не затрагиваются.</p>
          {loading && <Spinner label="Загрузка предварительного просмотра очистки" />}
          {preview && <>
            <h3 className="font-semibold">Удаление и сброс</h3><Counters value={preview.counts} labels={removedLabels} />
            <h3 className="font-semibold">Сохраняемые данные непрерывных исследований и конфигурация</h3><Counters value={preview.retained} labels={retainedLabels} />
            {(!preview.eligible || preview.blockers.length > 0) && <Alert variant="destructive" role="alert">{preview.blockers.length ? preview.blockers.map((blocker, index) => <p key={index}>{blockerMessage(blocker)}</p>) : "Сервер пока не разрешил очистку. Обновите предварительный просмотр."}</Alert>}
            {!hasChanges && <p className="text-sm">Истории непрерывных исследований для очистки нет.</p>}
          </>}
          {error && <Alert variant="destructive" role="alert">{error}</Alert>}
          <div><Button type="button" variant="outline" disabled={loading || deleting} onClick={reloadPreview}>Обновить предварительный просмотр</Button></div>
        </div>
        <div className="flex shrink-0 flex-wrap justify-end gap-2">
          <Button id={cancelId} type="button" variant="outline" disabled={deleting} onClick={closeModal}>Отмена</Button>
          <Button type="button" disabled={disabled || loading || deleting || !preview?.eligible || preview.blockers.length > 0 || !hasChanges} onClick={() => { void confirm(); }}>{deleting ? "Удаление…" : "Удалить историю"}</Button>
        </div>
      </DialogContent>
    </Dialog>
  </section>;
}
