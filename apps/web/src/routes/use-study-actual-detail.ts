import { useCallback, useContext, useEffect, useRef, useState } from "react";
import { getStudyActual, STUDY_ACTUAL_CHANGED, StudyActualApiError, type StudyActualDetail, type StudyActualSummary } from "../lib/study-actual-api";
import { errorText } from "./platform-controls";
import { RouteActiveContext } from "./route-active";

/** One open dialog owns one report. Even terminal states are observed for another tab's manual replacement. */
export function useStudyActualDetail(studyId: number, revisionTrigger: number) {
  const routeActive = useContext(RouteActiveContext);
  const [state, setState] = useState<{ actual: StudyActualDetail | null; error: string | null }>({ actual: null, error: null });
  const reloadRef = useRef<() => void>(() => undefined);
  const refresh = useCallback(() => { setState({ actual: null, error: null }); reloadRef.current(); }, []);
  useEffect(() => {
    if (!routeActive) return;
    let disposed = false; let minimumRevision = 0;
    let request: AbortController | undefined; let timer: ReturnType<typeof setTimeout> | undefined;
    async function load() {
      clearTimeout(timer); request?.abort();
      if (disposed || document.visibilityState === "hidden") return;
      const current = new AbortController(); request = current;
      let retry = true;
      try {
        const actual = await getStudyActual(studyId, current.signal);
        if (disposed || current.signal.aborted || actual.revision < minimumRevision) return;
        minimumRevision = actual.revision;
        setState((previous) => previous.actual?.revision === actual.revision && previous.actual.jobId === actual.jobId
          && previous.actual.status === "completed" && actual.status === "completed" && previous.error === null ? previous : { actual, error: null });
      } catch (cause) {
        if (disposed || current.signal.aborted) return;
        setState({ actual: null, error: errorText(cause) });
        retry = !(cause instanceof StudyActualApiError && (cause.status === 401 || cause.status === 404));
      } finally {
        if (!disposed && !current.signal.aborted && retry) timer = setTimeout(() => { void load(); }, 3000);
      }
    }
    const reload = () => { void load(); }; reloadRef.current = reload;
    const changed = (event: Event) => {
      const actual = (event as CustomEvent<StudyActualSummary>).detail;
      if (actual.studyId !== studyId || actual.revision < minimumRevision) return;
      minimumRevision = actual.revision; setState({ actual: null, error: null }); reload();
    };
    const visibility = () => { clearTimeout(timer); request?.abort(); if (document.visibilityState !== "hidden") { setState({ actual: null, error: null }); reload(); } };
    reload(); window.addEventListener(STUDY_ACTUAL_CHANGED, changed); document.addEventListener("visibilitychange", visibility);
    return () => { disposed = true; request?.abort(); clearTimeout(timer); window.removeEventListener(STUDY_ACTUAL_CHANGED, changed); document.removeEventListener("visibilitychange", visibility); if (reloadRef.current === reload) reloadRef.current = () => undefined; };
  }, [studyId, revisionTrigger, routeActive]);
  return { ...state, refresh };
}
