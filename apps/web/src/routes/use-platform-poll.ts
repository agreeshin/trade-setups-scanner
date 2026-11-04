import { useEffect, useState } from "react";
import { apiFetch, type PlatformState } from "../lib/platform-api";
import { errorText } from "./platform-controls";

// Each request belongs to this mounted view; hidden tabs neither poll nor retain in-flight requests.
export function usePlatformPoll<T>(path: string, revision = 0, intervalMs = 3000): PlatformState<T> {
  const [state, setState] = useState<PlatformState<T>>({ kind: "loading" });
  useEffect(() => {
    let disposed = false;
    let controller: AbortController | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function load() {
      if (disposed || document.visibilityState === "hidden") return;
      controller = new AbortController();
      const signal = controller.signal;
      try {
        const response = await apiFetch(path, { signal });
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        const data = await response.json() as T;
        if (!disposed && !signal.aborted) setState({ kind: "ready", data });
      } catch (cause) {
        if (!disposed && !signal.aborted) setState({ kind: "unreachable", detail: errorText(cause) });
      } finally {
        if (!disposed && !signal.aborted && intervalMs > 0) timer = setTimeout(() => { void load(); }, intervalMs);
      }
    }
    function visibility() {
      controller?.abort();
      clearTimeout(timer);
      if (document.visibilityState !== "hidden") void load();
    }
    void load();
    document.addEventListener("visibilitychange", visibility);
    return () => { disposed = true; controller?.abort(); clearTimeout(timer); document.removeEventListener("visibilitychange", visibility); };
  }, [path, revision, intervalMs]);
  return state;
}
