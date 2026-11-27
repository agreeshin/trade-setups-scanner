import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import { Alert } from "../components/ui/alert";
import { Button } from "../components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "../components/ui/dialog";
import { Spinner } from "../components/ui/spinner";
import { effectiveTimezone, formatIsoDateTime, formatUnix, interfaceSettings, subscribeInterfaceSettings } from "../lib/datetime-format";
import { fetchStudyPresentationPng, StudyPresentationApiError, type StudyPresentationSnapshot } from "../lib/study-presentation-api";
import { errorText } from "./platform-controls";
import { formatOutcomeRisk, outcomeMeanClass } from "./setup-outcomes-table";

function PresentationImage({ studyId, revision, onRefresh }: { studyId: number; revision: number; onRefresh: () => void }) {
  const [state, setState] = useState<{ url?: string; error?: string; stale?: boolean }>({});
  const [retry, setRetry] = useState(0);
  // At most one <=256 KiB image, owned by this open viewer, never a row/global cache.
  const resource = useRef<{ url?: string }>({});
  useEffect(() => {
    let disposed = false;
    let request: AbortController | undefined;
    const release = () => { if (resource.current.url) URL.revokeObjectURL(resource.current.url); resource.current = {}; };
    const load = () => {
      request?.abort(); release(); setState({});
      if (document.visibilityState === "hidden") return;
      const current = new AbortController(); request = current;
      void fetchStudyPresentationPng(studyId, revision, current.signal).then((blob) => {
        if (disposed || current.signal.aborted) return;
        const url = URL.createObjectURL(blob); resource.current.url = url; setState({ url });
      }).catch((cause) => {
        if (!disposed && !current.signal.aborted) setState({ error: errorText(cause), stale: cause instanceof StudyPresentationApiError && cause.code === "study_presentation_revision_changed" });
      });
    };
    const visibility = () => { request?.abort(); if (document.visibilityState === "hidden") { release(); setState({}); } else load(); };
    load(); document.addEventListener("visibilitychange", visibility);
    return () => { disposed = true; request?.abort(); release(); document.removeEventListener("visibilitychange", visibility); };
  }, [studyId, revision, retry]);
  if (state.error) return <Alert variant="destructive" role="alert">{state.error}<Button type="button" variant="outline" onClick={state.stale ? onRefresh : () => setRetry((value) => value + 1)}>{state.stale ? "Загрузить текущую версию PNG" : "Повторить загрузку PNG"}</Button></Alert>;
  if (!state.url) return <Spinner label="Загрузка сохранённого PNG исследования" />;
  return <img src={state.url} alt={`Сохранённый график матожидания исследования ${studyId}, ревизия ${revision}`} width={900} height={320} className="h-auto w-full min-w-0 rounded-md" onError={() => {
    if (resource.current.url) URL.revokeObjectURL(resource.current.url); resource.current = {};
    setState({ error: "Не удалось прочитать сохранённое PNG-изображение исследования." });
  }} />;
}

export function StudyPresentationDialog({ studyId, snapshot, returnFocusId, fallbackFocusId, onClose, onRefresh }: {
  studyId: number; snapshot: StudyPresentationSnapshot; returnFocusId: string; fallbackFocusId: string;
  onClose: () => void; onRefresh: () => void;
}) {
  useSyncExternalStore(subscribeInterfaceSettings, interfaceSettings);
  return <Dialog open onOpenChange={(open) => { if (!open) onClose(); }}>
    <DialogContent className="max-w-5xl" onCloseAutoFocus={(event) => {
      event.preventDefault();
      const trigger = document.getElementById(returnFocusId);
      (trigger && !trigger.matches(":disabled") ? trigger : document.getElementById(fallbackFocusId))?.focus();
    }}>
      <DialogHeader><DialogTitle>График PNG исследования #{studyId}</DialogTitle><DialogDescription>Последний полностью сохранённый серверный снимок. Просмотр не запускает расчёт. Новый пересчёт не меняет эту версию до успешной публикации.</DialogDescription></DialogHeader>
      <dl className="grid min-w-0 gap-2 text-sm sm:grid-cols-2">
        <div><dt className="text-muted-foreground">Источник</dt><dd>{snapshot.source === "initial" ? "Исходные данные исследования" : "Исходные данные и пересчёт после исследования"}</dd></div>
        <div><dt className="text-muted-foreground">Ревизия PNG / источника</dt><dd>{snapshot.revision} / {snapshot.sourceRevision ?? "исходный снимок"}</dd></div>
        <div><dt className="text-muted-foreground">Граница наблюдения</dt><dd>{formatUnix(snapshot.observationCutoff)} ({effectiveTimezone()})</dd></div>
        <div><dt className="text-muted-foreground">Опубликован</dt><dd>{formatIsoDateTime(snapshot.publishedAt)}</dd></div>
        <div><dt className="text-muted-foreground">Среднее матожидание снимка</dt><dd className={outcomeMeanClass(snapshot.meanR)}>{snapshot.meanR === null ? "Недоступно" : formatOutcomeRisk(snapshot.meanR)}</dd></div>
        <div><dt className="text-muted-foreground">Закрытые сделки / TP / SL</dt><dd>{snapshot.counts ? `${snapshot.counts.closed} / ${snapshot.counts.tp} / ${snapshot.counts.sl}` : "Недоступно"}</dd></div>
      </dl>
      <PresentationImage key={`${studyId}:${snapshot.revision}`} studyId={studyId} revision={snapshot.revision} onRefresh={onRefresh} />
      <p className="text-xs text-muted-foreground">Календарная ось PNG — UTC. Метаданные времени выше — в часовом поясе интерфейса. Диагностический снимок не является результатом исполнения реальных сделок.</p>
    </DialogContent>
  </Dialog>;
}
