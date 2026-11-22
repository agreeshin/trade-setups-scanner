import { useEffect, useSyncExternalStore } from "react";
import { fetchAnalyticsSettings, saveAnalyticsSettings, type AnalyticsUserSettings } from "./analytics-settings";
import { initialForecastRecipeDraft } from "./forecast-recipe";

interface SettingsState {
  status: "idle" | "loading" | "ready" | "error";
  settings: AnalyticsUserSettings | null;
  error: string | null;
  saveStatus: "idle" | "saving" | "saved" | "error";
  saveError: string | null;
}
const emptyState = (): SettingsState => ({ status: "idle", settings: null, error: null, saveStatus: "idle", saveError: null });
// One application-owned settings document, one replaceable draft and one serialized
// writer. Subscribers release their references; the last unmount clears the cache.
let state = emptyState();
const listeners = new Set<() => void>();
let epoch = 0;
let revision = 0;
let loading: AbortController | null = null;
let timer: ReturnType<typeof setTimeout> | undefined;
let writing: Promise<void> | null = null;
const message = (cause: unknown) => cause instanceof Error ? cause.message : "Не удалось сохранить настройки";
const publish = (next: SettingsState) => { state = next; for (const listener of listeners) listener(); };
const snapshot = () => state;

export function loadSharedAnalyticsSettings(): void {
  if (state.status === "loading" || state.status === "ready") return;
  const controller = new AbortController(); loading = controller;
  const started = epoch;
  publish({ ...state, status: "loading", error: null });
  void fetchAnalyticsSettings(controller.signal).then((saved) => {
    if (started !== epoch || controller.signal.aborted) return;
    const settings = saved ?? { setId: null, clusterize: true, maturity: 0.5, clusterGapInput: "8", autoCalc: false,
      trendBendDays: "linear", trendMode: "mean", trendBlendLambda: "0.7", forecast: initialForecastRecipeDraft(), sections: {} };
    publish({ ...state, settings, status: "ready" });
  }).catch((cause: unknown) => {
    if (started === epoch && !controller.signal.aborted) publish({ ...state, status: "error", error: message(cause) });
  }).finally(() => { if (loading === controller) loading = null; });
}

export function saveSharedAnalyticsSettings(): void {
  if (writing !== null || state.status !== "ready" || state.settings === null) return;
  if (timer !== undefined) { clearTimeout(timer); timer = undefined; }
  const started = epoch; const sent = revision; const settings = state.settings;
  publish({ ...state, saveStatus: "saving", saveError: null });
  const request = saveAnalyticsSettings(settings).then(() => {
    // An older ACK must never restore an older draft or report a newer edit saved.
    if (started === epoch && sent === revision) publish({ ...state, saveStatus: "saved", saveError: null });
  }).catch((cause: unknown) => {
    if (started === epoch && sent === revision) publish({ ...state, saveStatus: "error", saveError: message(cause) });
  }).finally(() => {
    if (writing === request) writing = null;
    if ((started === epoch && revision > sent) || (started !== epoch && revision > 0)) saveSharedAnalyticsSettings();
  });
  writing = request;
}

export function updateSharedAnalyticsSettings(patch: Partial<AnalyticsUserSettings>, saveDelay = 800): void {
  if (state.status !== "ready" || state.settings === null) return;
  const sections = { ...state.settings.sections, ...patch.sections };
  for (const key of ["short-matrix", "short-trades", "long-matrix", "long-trades"] as const) {
    if (patch.sections?.[key] !== undefined) sections[key] = { ...state.settings.sections[key], ...patch.sections[key] };
  }
  const settings = { ...state.settings, ...patch,
    sections };
  revision += 1;
  publish({ ...state, settings, saveStatus: "idle", saveError: null });
  if (timer !== undefined) clearTimeout(timer);
  timer = setTimeout(saveSharedAnalyticsSettings, saveDelay);
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
    // StrictMode's immediate remount retains the same owner/load, not a second GET.
    queueMicrotask(() => {
      if (listeners.size !== 0) return;
      epoch += 1; revision = 0; loading?.abort(); loading = null;
      if (timer !== undefined) clearTimeout(timer);
      timer = undefined; state = emptyState();
    });
  };
}

export function useSharedAnalyticsSettings(): SettingsState {
  const current = useSyncExternalStore(subscribe, snapshot, snapshot);
  useEffect(loadSharedAnalyticsSettings, []);
  return current;
}
