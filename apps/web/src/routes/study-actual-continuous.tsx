import { useContext, useEffect, useId, useMemo, useRef, useState } from "react";
import { Alert } from "../components/ui/alert";
import { Button } from "../components/ui/button";
import { DateInput } from "../components/ui/date-input";
import { Switch } from "../components/ui/switch";
import { effectiveTimezone } from "../lib/datetime-format";
import { isStudyActualDate, studyActualContinuous, type StudyActualContinuous } from "../lib/study-actual-continuous-api";
import { errorText } from "./platform-controls";
import { RouteActiveContext } from "./route-active";

/** One retained settings projection and one owned request; timers only READ server state. */
export function StudyActualContinuousControls({ algorithmId, active, onChanged }: { algorithmId: string; active: boolean; onChanged: () => void }) {
  const routeActive = useContext(RouteActiveContext);
  const labelId = useId();
  const scope = useMemo(() => ({ algorithmId, active, routeActive }), [algorithmId, active, routeActive]);
  const [snapshot, setSnapshot] = useState<StudyActualContinuous | null>(null);
  const [confirmedScope, setConfirmedScope] = useState<object | null>(null);
  const [savingScope, setSavingScope] = useState<object | null>(null);
  const available = confirmedScope === scope;
  const saving = savingScope === scope;
  const [error, setError] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const owner = useRef<{ refresh: () => void; save: (enabled: boolean, date: string) => void } | null>(null);
  const changed = useRef(onChanged);
  useEffect(() => { changed.current = onChanged; }, [onChanged]);
  useEffect(() => {
    if (!active || !routeActive) return;
    let disposed = false;
    let request: AbortController | null = null;
    let current: StudyActualContinuous | null = null;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let writing = false;
    let refreshQueued = false;
    function schedule() {
      clearTimeout(timer);
      if (disposed || document.visibilityState === "hidden") return;
      const queue = current?.queue;
      timer = setTimeout(() => { void load(); }, queue?.running || queue?.manualQueued || queue?.automaticQueued ? 3000 : 15000);
    }
    function accept(next: StudyActualContinuous, saved = false) {
      const previousSignature = current === null ? null : JSON.stringify([current.queue, current.cursorStudyId, current.cursorExecutionStartedAt]);
      const signature = JSON.stringify([next.queue, next.cursorStudyId, next.cursorExecutionStartedAt]);
      current = next; setSnapshot(next); setConfirmedScope(scope); setError(null);
      // The history owns its initial load. Only changes after this snapshot
      // invalidate its page; unchanged settings polls must remain quiet.
      if (saved || (previousSignature !== null && previousSignature !== signature)) changed.current();
    }
    async function load() {
      if (disposed || document.visibilityState === "hidden") return;
      clearTimeout(timer);
      if (request) { refreshQueued = true; return; }
      const reading = new AbortController(); request = reading;
      try {
        const next = await studyActualContinuous(algorithmId, reading.signal);
        if (!disposed && !reading.signal.aborted) accept(next);
      } catch (cause) {
        if (!disposed && !reading.signal.aborted) { setConfirmedScope(null); setError(errorText(cause)); }
      } finally {
        if (request === reading) request = null;
        if (!disposed && !reading.signal.aborted) {
          if (refreshQueued) { refreshQueued = false; void load(); } else schedule();
        }
      }
    }
    async function save(enabled: boolean, fromDate: string) {
      if (disposed || writing || !current) return;
      if (!isStudyActualDate(fromDate)) { setSaveError("Выберите корректную дату начала непрерывного пересчёта."); return; }
      if (current.enabled === enabled && current.fromDate === fromDate) return;
      // A mutation fences a preceding GET, even if transport ignores abort.
      request?.abort(); clearTimeout(timer); refreshQueued = false;
      const command = new AbortController(); request = command; writing = true;
      setSavingScope(scope); setSaveError(null);
      try {
        const next = await studyActualContinuous(algorithmId, command.signal, { enabled, fromDate, timeZone: effectiveTimezone(), revision: current.revision });
        if (!disposed && !command.signal.aborted) accept(next, true);
      } catch (cause) {
        if (!disposed && !command.signal.aborted) {
          setConfirmedScope(null); setSaveError(`${errorText(cause)} Показано последнее подтверждённое состояние; проверяем сохранение.`);
          // Unknown ACK and CAS conflicts are reconciled ONLY with GET. Never retry PUT.
          refreshQueued = true;
        }
      } finally {
        if (request === command) request = null;
        writing = false;
        if (!disposed && !command.signal.aborted) {
          setSavingScope(null);
          if (refreshQueued) { refreshQueued = false; void load(); } else schedule();
        }
      }
    }
    const controls = { refresh: () => { void load(); }, save: (enabled: boolean, date: string) => { void save(enabled, date); } };
    owner.current = controls;
    const visibility = () => {
      clearTimeout(timer);
      // Do not interrupt an admitted PUT when the browser becomes hidden.
      if (!writing) { request?.abort(); request = null; refreshQueued = false; }
      if (document.visibilityState !== "hidden") controls.refresh();
    };
    void load(); document.addEventListener("visibilitychange", visibility);
    return () => {
      disposed = true; request?.abort(); clearTimeout(timer);
      document.removeEventListener("visibilitychange", visibility);
      if (owner.current === controls) owner.current = null;
    };
  }, [algorithmId, active, routeActive, scope]);
  const state = snapshot?.algorithmId === algorithmId ? snapshot : null;
  const disabled = !active || !routeActive || !available || state === null || saving;
  return <div className="ml-auto grid min-w-0 gap-2" aria-busy={saving}>
    <div className="flex min-w-0 flex-wrap items-center gap-3">
      <label id={labelId} className="flex min-w-0 items-center gap-2 text-sm"><Switch checked={state?.enabled ?? false} disabled={disabled} onCheckedChange={(enabled) => { if (state) owner.current?.save(enabled, state.fromDate); }} />Включить непрерывный пересчёт факт матожидания</label>
      <DateInput type="date" aria-label="Дата начала непрерывного пересчёта факт матожидания" aria-describedby={labelId} min="1970-01-01" max="9999-12-31" className="flex h-9 w-44 min-w-0 items-center rounded-lg border border-input bg-background px-3 py-1 text-sm tabular-nums transition-colors hover:border-muted-foreground/50" value={state?.fromDate ?? ""} disabled={disabled} onChange={(event) => { if (state) owner.current?.save(state.enabled, event.target.value); }} />
    </div>
    {saving && <p role="status" className="text-xs text-muted-foreground">Сохранение настройки непрерывного пересчёта…</p>}
    {!available && <p role="status" className="text-xs text-muted-foreground">{state ? "Актуальность настройки не подтверждена. Показано последнее сохранённое состояние." : "Загрузка настройки непрерывного пересчёта…"}</p>}
    {state?.queue.running && <p role="status" className="text-xs text-muted-foreground">Выполняется пересчёт исследования #{state.queue.running.studyId} · {state.queue.running.requestOrigin === "automatic" ? "автоматически" : "вручную"}</p>}
    {state && (state.queue.manualQueued > 0 || state.queue.automaticQueued > 0) && <p className="text-xs text-muted-foreground">В очереди: ручных {state.queue.manualQueued}, автоматических {state.queue.automaticQueued}</p>}
    {(error ?? saveError) && <Alert variant="destructive" role="alert">{saveError ?? error}<Button type="button" variant="outline" size="sm" disabled={saving} onClick={() => { setSaveError(null); owner.current?.refresh(); }}>Обновить настройку пересчёта</Button></Alert>}
  </div>;
}
