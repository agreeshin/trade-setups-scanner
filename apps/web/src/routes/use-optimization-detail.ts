import { useContext, useEffect, useState } from "react";
import { getOptimizationStudy, isActiveStudy, OptimizationApiError, type OptimizationStudy } from "../lib/optimization-api";
import { errorText } from "./platform-controls";
import { RouteActiveContext } from "./route-active";

interface DetailState { id: number; study: OptimizationStudy | null; error: string | null }

/** An open dialog owns exactly one full record. Selection/close/unmount aborts GETs,
 * disposes observers and releases the previous matrix; it never cancels a study. */
export function useOptimizationDetail(id: number | undefined, open: boolean, revision: number, terminalVersion?: string) {
  const routeActive = useContext(RouteActiveContext);
  const [state, setState] = useState<DetailState | null>(null);
  useEffect(() => {
    setState(null);
    if (!open || id === undefined || !routeActive) return;
    const selectedId = id;
    let disposed = false;
    let request: AbortController | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function load() {
      request?.abort(); clearTimeout(timer);
      if (disposed || document.visibilityState === "hidden") return;
      const current = new AbortController(); request = current;
      try {
        const study = await getOptimizationStudy(selectedId, current.signal);
        if (disposed || current.signal.aborted) return;
        setState({ id: selectedId, study, error: null });
        if (isActiveStudy(study)) timer = setTimeout(() => { void load(); }, 3000);
      } catch (cause) {
        if (disposed || current.signal.aborted) return;
        setState((previous) => ({ id: selectedId, study: previous?.id === selectedId ? previous.study : null, error: errorText(cause) }));
        if (!(cause instanceof OptimizationApiError && cause.status === 404)) timer = setTimeout(() => { void load(); }, 3000);
      }
    }
    const visibility = () => { request?.abort(); clearTimeout(timer); if (document.visibilityState !== "hidden") void load(); };
    void load(); document.addEventListener("visibilitychange", visibility);
    return () => { disposed = true; request?.abort(); clearTimeout(timer); document.removeEventListener("visibilitychange", visibility); };
  }, [id, open, revision, terminalVersion, routeActive]);
  return state?.id === id ? state : null;
}
