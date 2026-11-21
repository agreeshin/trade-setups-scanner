import { useCallback, useContext, useEffect, useRef, useState, useSyncExternalStore } from "react";
import { effectiveTimezone, interfaceSettings, subscribeInterfaceSettings } from "../lib/datetime-format";
import { getStudyActual, STUDY_ACTUAL_CHANGED, StudyActualApiError, type StudyActualDetail, type StudyActualReport, type StudyActualSummary } from "../lib/study-actual-api";
import { errorText } from "./platform-controls";
import { RouteActiveContext } from "./route-active";

/** One open dialog owns one report. Even terminal states are observed for another tab's manual replacement. */
export function useStudyActualDetail(studyId: number, revisionTrigger: number) {
  const routeActive = useContext(RouteActiveContext);
  useSyncExternalStore(subscribeInterfaceSettings, interfaceSettings);
  const timeZone = effectiveTimezone();
  const [state, setState] = useState<{ studyId: number; timeZone: string; actual: StudyActualDetail | null; originalReport: StudyActualReport | null; error: string | null }>({ studyId, timeZone, actual: null, originalReport: null, error: null });
  // One selected-study fence/report pair, released on selection or disposal; old-zone charts stay hidden while reloading.
  const fence = useRef({ studyId, revision: 0 });
  const reloadRef = useRef<() => void>(() => undefined);
  const refresh = useCallback(() => { setState((previous) => ({ ...previous, actual: null, error: null })); reloadRef.current(); }, []);
  useEffect(() => {
    setState((previous) => previous.studyId === studyId ? previous : { studyId, timeZone, actual: null, originalReport: null, error: null });
    if (fence.current.studyId !== studyId) fence.current = { studyId, revision: 0 };
    if (!routeActive) return;
    let disposed = false;
    let request: AbortController | undefined; let timer: ReturnType<typeof setTimeout> | undefined;
    async function load() {
      clearTimeout(timer); request?.abort();
      if (disposed || document.visibilityState === "hidden") return;
      const current = new AbortController(); request = current;
      let retry = true;
      try {
        const actual = await getStudyActual(studyId, current.signal, timeZone);
        if (disposed || current.signal.aborted || actual.revision < fence.current.revision) return;
        fence.current.revision = actual.revision;
        setState({ studyId, timeZone, actual, originalReport: actual.originalReport ?? null, error: null });
      } catch (cause) {
        if (disposed || current.signal.aborted) return;
        const protectedFailure = cause instanceof StudyActualApiError && (cause.status === 401 || cause.status === 403 || cause.status === 404);
        setState((previous) => ({ studyId, timeZone, actual: null,
          originalReport: !protectedFailure && previous.studyId === studyId && previous.timeZone === timeZone ? previous.originalReport : null,
          error: errorText(cause) }));
        retry = !protectedFailure;
      } finally {
        if (!disposed && !current.signal.aborted && retry) timer = setTimeout(() => { void load(); }, 3000);
      }
    }
    const reload = () => { void load(); }; reloadRef.current = reload;
    const changed = (event: Event) => {
      const actual = (event as CustomEvent<StudyActualSummary>).detail;
      if (actual.studyId !== studyId || actual.revision < fence.current.revision) return;
      fence.current.revision = actual.revision; setState((previous) => ({ ...previous, actual: null, error: null })); reload();
    };
    const visibility = () => { clearTimeout(timer); request?.abort(); if (document.visibilityState !== "hidden") { refresh(); } };
    reload(); window.addEventListener(STUDY_ACTUAL_CHANGED, changed); document.addEventListener("visibilitychange", visibility);
    return () => { disposed = true; request?.abort(); clearTimeout(timer); window.removeEventListener(STUDY_ACTUAL_CHANGED, changed); document.removeEventListener("visibilitychange", visibility); if (reloadRef.current === reload) reloadRef.current = () => undefined; };
  }, [studyId, revisionTrigger, routeActive, timeZone, refresh]);
  const selected = state.studyId === studyId;
  const matchingZone = selected && state.timeZone === timeZone;
  return { actual: selected ? state.actual : null,
    originalReport: matchingZone ? state.originalReport : null,
    report: matchingZone ? state.actual?.report ?? state.originalReport : null,
    error: matchingZone ? state.error : null,
    loading: !matchingZone || (state.actual === null && state.error === null), refresh };
}
