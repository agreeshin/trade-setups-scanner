import { useCallback, useContext, useEffect, useRef, useState } from "react";
import {
  cancelOptimizationStudy, createOptimizationStudy, getOptimizationStudy, isActiveStudy,
  listOptimizationStudies, OptimizationApiError, type OptimizationSpec, type OptimizationStudy,
} from "../lib/optimization-api";
import { errorText } from "./platform-controls";
import { RouteActiveContext } from "./route-active";

interface StudySnapshot { byAlgorithm: Record<string, OptimizationStudy>; active: OptimizationStudy | null }

/** Only snapshots belong to this view. Timers/GETs are disposed, never the server study.
 * Retention is one record per registered algorithm plus the single global active study. */
export function useOptimizationStudies(algorithmIds: string[]) {
  const idsKey = JSON.stringify(algorithmIds);
  const routeActive = useContext(RouteActiveContext);
  const [snapshot, setSnapshot] = useState<StudySnapshot>({ byAlgorithm: {}, active: null });
  const snapshotRef = useRef<StudySnapshot>({ byAlgorithm: {}, active: null });
  const [ready, setReady] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [cancelling, setCancelling] = useState<number | null>(null);
  const mounted = useRef(false);
  const version = useRef(0);
  const mutation = useRef<AbortController | null>(null);
  const refreshRef = useRef<() => void>(() => undefined);
  const activeRef = useRef<OptimizationStudy | null>(null);
  const readyRef = useRef(false);

  const accept = useCallback((study: OptimizationStudy) => {
    const active = isActiveStudy(study) ? study : null;
    activeRef.current = active;
    readyRef.current = true;
    setReady(true);
    const next = { byAlgorithm: { ...snapshotRef.current.byAlgorithm, [study.spec.algorithmId]: study }, active };
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
          ...ids.map((id) => listOptimizationStudies(id, current.signal)),
        ]);
        const activeSummary = global?.find(isActiveStudy);
        const selected = lists.flatMap((list) => {
          const ordered = [...list].sort((a, b) => Number(isActiveStudy(b)) - Number(isActiveStudy(a)) || b.id - a.id);
          return ordered[0] ? [ordered[0]] : [];
        });
        if (activeSummary && !selected.some((study) => study.id === activeSummary.id)) selected.push(activeSummary);
        const details = await Promise.all(selected.map((study) => {
          const cached = snapshotRef.current.byAlgorithm[study.spec.algorithmId];
          // Terminal matrices can contain 4096 cells. Keep one snapshot and only fetch again
          // for a new/changed record; active studies always get authoritative detail updates.
          if (!isActiveStudy(study) && cached?.id === study.id && cached.status === study.status && cached.updatedAt === study.updatedAt) return Promise.resolve(cached);
          return getOptimizationStudy(study.id, current.signal);
        }));
        if (disposed || current.signal.aborted || revision !== version.current) return;
        const active = details.find(isActiveStudy) ?? null;
        const byAlgorithm: Record<string, OptimizationStudy> = {};
        for (const study of details) if (ids.includes(study.spec.algorithmId)) byAlgorithm[study.spec.algorithmId] = study;
        activeRef.current = active; readyRef.current = true;
        const next = { byAlgorithm, active }; snapshotRef.current = next;
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
      if (mounted.current && !controller.signal.aborted) { version.current += 1; accept(study); return true; }
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
  async function stop(study: OptimizationStudy) {
    if (mutation.current || !isActiveStudy(study) || study.status === "cancel_requested") return;
    const controller = new AbortController(); mutation.current = controller;
    version.current += 1; setCancelling(study.id); setError(null);
    try {
      const next = await cancelOptimizationStudy(study.id, controller.signal);
      if (mounted.current && !controller.signal.aborted) { version.current += 1; accept(next); }
    } catch (cause) {
      if (mounted.current && !controller.signal.aborted) setError(errorText(cause));
    } finally {
      if (mutation.current === controller) mutation.current = null;
      if (mounted.current && !controller.signal.aborted) setCancelling(null);
    }
  }
  return { ...snapshot, ready, error, creating, cancelling, start, stop, refresh: () => refreshRef.current() };
}
export type OptimizationController = ReturnType<typeof useOptimizationStudies>;
