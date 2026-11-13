import { useCallback, useContext, useEffect, useRef, useState } from "react";
import {
  cancelOptimizationStudy, createOptimizationStudy, deleteOptimizationStudy, getOptimizationStudy, isActiveStudy, isContinuousStudy,
  listOptimizationStudies, OptimizationApiError, summarizeOptimizationStudy, type OptimizationSpec, type OptimizationStudy, type OptimizationStudyAction, type OptimizationStudySummary,
} from "../lib/optimization-api";
import { errorText } from "./platform-controls";
import { RouteActiveContext } from "./route-active";

interface StudySnapshot { byAlgorithm: Record<string, OptimizationStudySummary>; latestByAlgorithm: Record<string, OptimizationStudySummary>; active: OptimizationStudySummary | null }

/** Retain latest manual and latest any-origin compact summaries per registered algorithm, plus the active manual summary.
 * Full detail belongs only to an open modal, never to discovery or the history table. */
export function useOptimizationStudies(algorithmIds: string[]) {
  const idsKey = JSON.stringify(algorithmIds);
  const routeActive = useContext(RouteActiveContext);
  const [snapshot, setSnapshot] = useState<StudySnapshot>({ byAlgorithm: {}, latestByAlgorithm: {}, active: null });
  const snapshotRef = useRef<StudySnapshot>({ byAlgorithm: {}, latestByAlgorithm: {}, active: null });
  const [ready, setReady] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [cancelling, setCancelling] = useState<number | null>(null);
  const [deleting, setDeleting] = useState<number | null>(null);
  const [revision, setRevision] = useState(0);
  const mounted = useRef(false);
  const version = useRef(0);
  const mutation = useRef<AbortController | null>(null);
  const refreshRef = useRef<() => void>(() => undefined);
  const activeRef = useRef<OptimizationStudySummary | null>(null);
  const readyRef = useRef(false);

  const accept = useCallback((study: OptimizationStudy) => {
    const summary = summarizeOptimizationStudy(study);
    const previous = snapshotRef.current;
    const active = isActiveStudy(summary) && !isContinuousStudy(summary) ? summary : previous.active?.id === summary.id ? null : previous.active;
    activeRef.current = active;
    readyRef.current = true;
    setReady(true);
    const existing = previous.byAlgorithm[study.spec.algorithmId];
    const replace = !existing || existing.id === study.id || isActiveStudy(summary) || (!isActiveStudy(existing) && study.id > existing.id);
    const next = { byAlgorithm: replace && !isContinuousStudy(summary) ? { ...previous.byAlgorithm, [study.spec.algorithmId]: summary } : previous.byAlgorithm,
      latestByAlgorithm: { ...previous.latestByAlgorithm, [study.spec.algorithmId]: summary }, active };
    snapshotRef.current = next; setSnapshot(next);
  }, []);

  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; mutation.current?.abort(); };
  }, []);

  useEffect(() => {
    if (!routeActive) return;
    const ids = JSON.parse(idsKey) as string[];
    let disposed = false;
    let controller: AbortController | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function load() {
      if (disposed || document.visibilityState === "hidden") return;
      controller?.abort(); clearTimeout(timer);
      const current = new AbortController(); controller = current;
      const revision = version.current;
      try {
        const [global, ...lists] = await Promise.all([
          listOptimizationStudies(undefined, current.signal),
          // TODO: use a compact origin-filtered latest-manual query when available. This bounded window
          // can age out an old manual summary; its full history remains available in the history dialog.
          ...ids.map((id) => listOptimizationStudies(id, current.signal, 20)),
        ]);
        const activeSummary = global.studies.find((study) => isActiveStudy(study) && !isContinuousStudy(study));
        const selected = lists.flatMap((list) => {
          const ordered = [...list.studies].sort((a, b) => Number(isActiveStudy(b)) - Number(isActiveStudy(a)) || b.id - a.id);
          const manual = ordered.find((study) => !isContinuousStudy(study));
          return ordered[0] ? manual && manual.id !== ordered[0].id ? [ordered[0], manual] : [ordered[0]] : [];
        });
        if (activeSummary && !selected.some((study) => study.id === activeSummary.id)) selected.push(activeSummary);
        if (disposed || current.signal.aborted || revision !== version.current) return;
        const active = selected.find((study) => isActiveStudy(study) && !isContinuousStudy(study)) ?? null;
        const byAlgorithm: Record<string, OptimizationStudySummary> = {};
        const latestByAlgorithm: Record<string, OptimizationStudySummary> = {};
        for (const study of selected) if (ids.includes(study.spec.algorithmId)) {
          latestByAlgorithm[study.spec.algorithmId] ??= study;
          if (!isContinuousStudy(study)) byAlgorithm[study.spec.algorithmId] = study;
        }
        activeRef.current = active; readyRef.current = true;
        const next = { byAlgorithm, latestByAlgorithm, active }; snapshotRef.current = next;
        setSnapshot(next); setReady(true); setError(null);
      } catch (cause) {
        if (!disposed && !current.signal.aborted && revision === version.current) {
          setError(`Статус оптимизации недоступен: ${errorText(cause)}. Последний снимок сохранён.`);
          readyRef.current = false; setReady(false);
        }
      } finally {
        if (!disposed && !current.signal.aborted) timer = setTimeout(() => { void load(); }, 3000);
      }
    }
    const refresh = () => { void load(); };
    refreshRef.current = refresh;
    function visibility() {
      controller?.abort(); clearTimeout(timer);
      if (document.visibilityState !== "hidden") refresh();
    }
    refresh(); document.addEventListener("visibilitychange", visibility);
    return () => {
      disposed = true; controller?.abort(); clearTimeout(timer);
      document.removeEventListener("visibilitychange", visibility);
      if (refreshRef.current === refresh) refreshRef.current = () => undefined;
    };
  }, [idsKey, routeActive]);

  async function start(spec: OptimizationSpec): Promise<boolean> {
    if (mutation.current || activeRef.current || !readyRef.current) return false;
    const controller = new AbortController(); mutation.current = controller;
    version.current += 1; setCreating(true); setError(null);
    try {
      const study = await createOptimizationStudy(spec, controller.signal);
      if (mounted.current && !controller.signal.aborted) { version.current += 1; accept(study); setRevision((value) => value + 1); refreshRef.current(); return true; }
    } catch (cause) {
      if (mounted.current && !controller.signal.aborted) {
        if (cause instanceof OptimizationApiError && cause.activeStudyId !== undefined) {
          try {
            const active = await getOptimizationStudy(cause.activeStudyId, controller.signal);
            if (!mounted.current || controller.signal.aborted) return false;
            version.current += 1; accept(active);
          } catch (detailCause) {
            if (!mounted.current || controller.signal.aborted) return false;
            readyRef.current = false; setReady(false);
            setError(`Исследование #${cause.activeStudyId} уже выполняется, но его статус недоступен: ${errorText(detailCause)}. Обновите статус перед новым запуском.`);
            return false;
          }
        } else {
          // A lost create acknowledgement can still mean a persisted job. Discover before retry.
          readyRef.current = false; setReady(false); refreshRef.current();
        }
        setError(errorText(cause));
      }
    } finally {
      if (mutation.current === controller) mutation.current = null;
      if (mounted.current && !controller.signal.aborted) setCreating(false);
    }
    return false;
  }
  async function stop(study: OptimizationStudyAction) {
    if (mutation.current || isContinuousStudy(study) || !isActiveStudy(study) || study.status === "cancel_requested") return;
    const controller = new AbortController(); mutation.current = controller;
    version.current += 1; setCancelling(study.id); setError(null);
    try {
      const next = await cancelOptimizationStudy(study.id, controller.signal);
      if (mounted.current && !controller.signal.aborted) { version.current += 1; accept(next); setRevision((value) => value + 1); refreshRef.current(); }
    } catch (cause) {
      if (mounted.current && !controller.signal.aborted) setError(errorText(cause));
    } finally {
      if (mutation.current === controller) mutation.current = null;
      if (mounted.current && !controller.signal.aborted) setCancelling(null);
    }
  }
  async function remove(study: OptimizationStudyAction): Promise<boolean> {
    if (mutation.current || isActiveStudy(study)) return false;
    const controller = new AbortController(); mutation.current = controller;
    version.current += 1; setDeleting(study.id); setError(null);
    try {
      await deleteOptimizationStudy(study.id, controller.signal);
      if (!mounted.current || controller.signal.aborted) return false;
      version.current += 1;
      const previous = snapshotRef.current;
      const next = { ...previous, byAlgorithm: Object.fromEntries(Object.entries(previous.byAlgorithm).filter(([, item]) => item.id !== study.id)),
        latestByAlgorithm: Object.fromEntries(Object.entries(previous.latestByAlgorithm).filter(([, item]) => item.id !== study.id)) };
      snapshotRef.current = next; setSnapshot(next);
      readyRef.current = false; setReady(false);
      setRevision((value) => value + 1); refreshRef.current();
      return true;
    } catch (cause) {
      if (mounted.current && !controller.signal.aborted) setError(errorText(cause));
    } finally {
      if (mutation.current === controller) mutation.current = null;
      if (mounted.current && !controller.signal.aborted) setDeleting(null);
    }
    return false;
  }
  const refresh = useCallback(() => { setRevision((value) => value + 1); refreshRef.current(); }, []);
  return { ...snapshot, ready, error, creating, cancelling, deleting, revision, start, stop, remove,
    clearError: () => setError(null), refresh };
}
export type OptimizationController = ReturnType<typeof useOptimizationStudies>;
