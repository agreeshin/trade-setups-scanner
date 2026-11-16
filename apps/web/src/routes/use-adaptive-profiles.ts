import { useCallback, useContext, useEffect, useRef, useState } from "react";
import { adaptivePermission, commandAdaptiveProfile, listAdaptiveProfiles, type AdaptiveProfile, type ProfileCommand, type ProfilePage } from "../lib/adaptive-profiles-api";
import { RouteActiveContext } from "./route-active";
import { errorText } from "./platform-controls";

type Action = "stop" | "play";
/** One bounded page plus per-visible-row requests. No scientific payload/history in RAM. */
export function useAdaptiveProfiles(algorithmId: string, active = true, refreshRevision = 0) {
  const routeActive = useContext(RouteActiveContext);
  const [page, setPage] = useState<ProfilePage | null>(null);
  const [permission, setPermission] = useState<boolean | null>(null);
  const [available, setAvailable] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [permissionError, setPermissionError] = useState<string | null>(null);
  const [rowErrors, setRowErrors] = useState<Record<number, string>>({});
  const [busy, setBusy] = useState<Record<number, Action>>({});
  const [permissionBusy, setPermissionBusy] = useState(false);
  const [offset, setOffset] = useState(0);
  const [revision, setRevision] = useState(0);
  const requests = useRef(new Map<number | "permission", AbortController>());
  const retries = useRef(new Map<number, { action: Action; command: ProfileCommand }>());
  const version = useRef(0);
  const mounted = useRef(false);
  const refresh = useCallback(() => setRevision((n) => n + 1), []);
  useEffect(() => {
    mounted.current = true;
    const owned = requests.current; const retained = retries.current;
    return () => { mounted.current = false; for (const request of owned.values()) request.abort(); owned.clear(); retained.clear(); };
  }, []);
  useEffect(() => {
    if (!active || !routeActive) return;
    let disposed = false;
    let request: AbortController | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function load() {
      if (disposed || document.visibilityState === "hidden") return;
      const current = new AbortController(); request = current;
      const observed = version.current;
      try {
        // Serial, independent endpoints; neither GET starts any work.
        const next = await listAdaptiveProfiles(algorithmId, offset, current.signal);
        const enabled = await adaptivePermission(algorithmId, current.signal);
        if (disposed || current.signal.aborted || observed !== version.current) return;
        setPage(next); setPermission(enabled); setAvailable(true); setError(null);
        setRowErrors((rows) => Object.fromEntries(Object.entries(rows).filter(([id]) => next.profiles.some((p) => p.id === Number(id)))));
        // Retain at most one retry key per row of the current page.
        for (const [id, retry] of retries.current) {
          const profile = next.profiles.find((p) => p.id === id);
          if (profile?.controlRevision !== retry.command.expectedRevision || profile.acceptedGenerationId !== retry.command.expectedGenerationId) retries.current.delete(id);
        }
        setBusy((previous) => Object.fromEntries(Object.entries(previous).filter(([id]) => requests.current.has(Number(id)))));
        timer = setTimeout(() => { void load(); }, next.profiles.some((p) => p.desiredRunning || p.status === "stopping") ? 3000 : 15000);
      } catch (cause) {
        if (!disposed && !current.signal.aborted && observed === version.current) { setAvailable(false); setError(errorText(cause)); setBusy({}); }
      } finally {
        if (!disposed && !current.signal.aborted) setLoading(false);
      }
    }
    const visibility = () => { request?.abort(); clearTimeout(timer); if (document.visibilityState !== "hidden") void load(); };
    void load(); document.addEventListener("visibilitychange", visibility);
    return () => { disposed = true; request?.abort(); clearTimeout(timer); document.removeEventListener("visibilitychange", visibility); };
  }, [algorithmId, active, routeActive, offset, revision, refreshRevision]);

  async function command(profile: AdaptiveProfile, action: Action) {
    if (!available || requests.current.has(profile.id)) return;
    const current = new AbortController(); requests.current.set(profile.id, current);
    const previous = retries.current.get(profile.id);
    const command = previous?.action === action && previous.command.expectedRevision === profile.controlRevision && previous.command.expectedGenerationId === profile.acceptedGenerationId
      ? previous.command : { expectedRevision: profile.controlRevision, expectedGenerationId: profile.acceptedGenerationId, requestId: crypto.randomUUID() };
    retries.current.set(profile.id, { action, command });
    version.current += 1; setBusy((rows) => ({ ...rows, [profile.id]: action })); setRowErrors((rows) => { const next = { ...rows }; delete next[profile.id]; return next; });
    try {
      await commandAdaptiveProfile(profile, action, command, current.signal);
      // A receipt confirms intent, NOT ACTIVE. Keep the saved row until GET.
    } catch (cause) {
      if (mounted.current && !current.signal.aborted) setRowErrors((rows) => ({ ...rows, [profile.id]: errorText(cause) }));
    } finally {
      requests.current.delete(profile.id);
      if (mounted.current && !current.signal.aborted) { setAvailable(false); refresh(); }
    }
  }
  async function savePermission(enabled: boolean): Promise<boolean> {
    if (!available || requests.current.has("permission")) return false;
    const current = new AbortController(); requests.current.set("permission", current); setPermissionBusy(true); setPermissionError(null); version.current += 1;
    try {
      const confirmed = await adaptivePermission(algorithmId, current.signal, enabled);
      if (!mounted.current || current.signal.aborted) return false;
      setPermission(confirmed); return true;
    } catch (cause) {
      if (mounted.current && !current.signal.aborted) setPermissionError(errorText(cause));
      return false;
    } finally {
      requests.current.delete("permission");
      if (mounted.current && !current.signal.aborted) { setPermissionBusy(false); refresh(); }
    }
  }
  return { page, permission, available, loading, error: error ?? permissionError, rowErrors, busy, permissionBusy, offset, setOffset, refresh, command, savePermission };
}
export type AdaptiveProfilesController = ReturnType<typeof useAdaptiveProfiles>;
