import { useCallback, useContext, useEffect, useRef, useState } from "react";
import { isContinuousWorkActive, listContinuousStudies, saveContinuousStudy, startContinuousStudy, stopContinuousStudy, type ContinuousConfig, type ContinuousProcess } from "../lib/continuous-api";
import { errorText } from "./platform-controls";
import { RouteActiveContext } from "./route-active";

/** One replaceable process snapshot per algorithm. Polling only observes server work. */
export function useContinuousStudy(algorithmId: string, refreshRevision = 0) {
  const routeActive = useContext(RouteActiveContext);
  const [process, setProcess] = useState<ContinuousProcess | null>(null);
  const [available, setAvailable] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<"save" | "start" | "stop" | null>(null);
  const [revision, setRevision] = useState(0);
  const mounted = useRef(false);
  const version = useRef(0);
  const mutation = useRef<AbortController | null>(null);
  const observed = useRef(false);
  const snapshot = useRef<ContinuousProcess | null>(null);
  const refreshRef = useRef<() => void>(() => undefined);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; mutation.current?.abort(); };
  }, []);
  useEffect(() => {
    if (!routeActive) return;
    let disposed = false;
    let request: AbortController | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function load() {
      if (disposed || document.visibilityState === "hidden") return;
      request?.abort(); clearTimeout(timer);
      const current = new AbortController(); request = current;
      const observedVersion = version.current;
      let succeeded = false;
      try {
        const processes = await listContinuousStudies(algorithmId, current.signal);
        if (disposed || current.signal.aborted || observedVersion !== version.current) return;
        const next = processes[0] ?? null;
        if (observed.current && (snapshot.current?.status !== next?.status || snapshot.current?.activeStudyId !== next?.activeStudyId)) setRevision((value) => value + 1);
        observed.current = true; snapshot.current = next;
        setProcess(next); setAvailable(true); setError(null); succeeded = true;
      } catch (cause) {
        if (!disposed && !current.signal.aborted && observedVersion === version.current) {
          setAvailable(false); setError(errorText(cause));
        }
      } finally {
        if (!disposed && !current.signal.aborted && observedVersion === version.current) {
          setLoading(false);
          // Unsupported endpoints wait for an explicit retry; do not generate an error-poll storm.
          if (succeeded) timer = setTimeout(() => { void load(); }, isContinuousWorkActive(snapshot.current) ? 3000 : 15000);
        }
      }
    }
    const refresh = () => { void load(); };
    refreshRef.current = refresh;
    const visibility = () => { request?.abort(); clearTimeout(timer); if (document.visibilityState !== "hidden") refresh(); };
    refresh(); document.addEventListener("visibilitychange", visibility);
    return () => {
      disposed = true; request?.abort(); clearTimeout(timer); document.removeEventListener("visibilitychange", visibility);
      if (refreshRef.current === refresh) refreshRef.current = () => undefined;
    };
  }, [algorithmId, routeActive, refreshRevision]);

  async function command(action: "save" | "start" | "stop", config?: ContinuousConfig): Promise<boolean> {
    if (action === "save" && !config) throw new Error("Continuous save requires its configuration.");
    if (!available || mutation.current) return false;
    const current = new AbortController(); mutation.current = current;
    version.current += 1; setBusy(action); setError(null);
    try {
      const next = action === "save" && config ? await saveContinuousStudy(algorithmId, config, current.signal)
        : action === "start" ? await startContinuousStudy(algorithmId, current.signal) : await stopContinuousStudy(algorithmId, current.signal);
      if (!mounted.current || current.signal.aborted) return false;
      version.current += 1; observed.current = true; snapshot.current = next; setProcess(next); setAvailable(true); setRevision((value) => value + 1);
      refreshRef.current(); return true;
    } catch (cause) {
      if (mounted.current && !current.signal.aborted) {
        setError(`${errorText(cause)} Последний подтверждённый снимок сохранён; обновите статус.`);
      }
    } finally {
      if (mutation.current === current) mutation.current = null;
      if (mounted.current && !current.signal.aborted) setBusy(null);
    }
    return false;
  }
  const refresh = useCallback(() => refreshRef.current(), []);
  return { process, available, loading, error, busy, revision, refresh,
    save: (config: ContinuousConfig) => command("save", config), start: () => command("start"), stop: () => command("stop") };
}
export type ContinuousController = ReturnType<typeof useContinuousStudy>;
