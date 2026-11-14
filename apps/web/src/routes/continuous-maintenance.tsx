import { useEffect, useRef, useState } from "react";
import { Alert } from "../components/ui/alert";
import { Badge } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import { SectionHeader } from "../components/ui/page-header";
import { Spinner } from "../components/ui/spinner";
import { ContinuousMaintenanceApiError, drainContinuousMaintenance, getContinuousMaintenance, resumeContinuousMaintenance, type ContinuousMaintenanceAction, type ContinuousMaintenanceStatus } from "../lib/continuous-maintenance-api";
import { formatIsoDateTime } from "../lib/datetime-format";
import { errorText } from "./platform-controls";
import { useRouteActive } from "./route-active";

const labels: Record<ContinuousMaintenanceStatus["state"], string> = {
  running: "Допуск новых непрерывных цепочек открыт",
  draining: "Допуск закрыт · ожидаем полного завершения цепочек",
  drained: "Допуск закрыт · непрерывные цепочки завершены",
};

/** Owns one replaceable status and one request. Observes maintenance; never drives chain lifetime. */
export function ContinuousMaintenance({ active }: { active: boolean }) {
  const routeActive = useRouteActive();
  const [snapshot, setSnapshot] = useState<ContinuousMaintenanceStatus | null>(null);
  const snapshotRef = useRef<ContinuousMaintenanceStatus | null>(null);
  const [available, setAvailable] = useState(false);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState<ContinuousMaintenanceAction | null>(null);
  const [readError, setReadError] = useState<string | null>(null);
  const [commandError, setCommandError] = useState<string | null>(null);
  const operations = useRef<{ refresh: () => void; command: (action: ContinuousMaintenanceAction) => void } | null>(null);

  useEffect(() => {
    if (!active || !routeActive) return;
    let disposed = false;
    let confirmed = false;
    let inFlight: { controller: AbortController; action: ContinuousMaintenanceAction | undefined } | null = null;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let refreshPending = false;
    const visible = () => !disposed && document.visibilityState !== "hidden";

    async function run(action?: ContinuousMaintenanceAction) {
      if (!visible()) return;
      if (inFlight) { if (!action) refreshPending = true; return; }
      if (action && !confirmed) return;
      clearTimeout(timer); refreshPending = false;
      const current = { controller: new AbortController(), action }; inFlight = current;
      if (action) { confirmed = false; setAvailable(false); setBusy(action); setCommandError(null); }
      else { setLoading(true); setAvailable(confirmed); setBusy(null); }
      let succeeded = false;
      let retryRead = false;
      try {
        const next = action === "drain" ? await drainContinuousMaintenance(current.controller.signal)
          : action === "resume" ? await resumeContinuousMaintenance(current.controller.signal) : await getContinuousMaintenance(current.controller.signal);
        if (disposed || current.controller.signal.aborted) return;
        snapshotRef.current = next; setSnapshot(next); confirmed = true;
        setAvailable(visible()); setReadError(null); succeeded = true;
      } catch (cause) {
        if (!disposed && !current.controller.signal.aborted) {
          confirmed = false; setAvailable(false);
          retryRead = !(cause instanceof ContinuousMaintenanceApiError && [401, 403, 404, 405, 501].includes(cause.status ?? 0));
          if (action) setCommandError(`${errorText(cause)} Команда не подтверждена; запрошен актуальный статус.`);
          else setReadError(errorText(cause));
        }
      } finally {
        if (inFlight === current) inFlight = null;
        if (!disposed) {
          if (action) { setBusy(null); refreshPending = true; }
          else setLoading(false);
          if (visible() && refreshPending) { refreshPending = false; void run(); }
          else if (visible() && !current.controller.signal.aborted && (succeeded || retryRead)) {
            const latest = snapshotRef.current;
            timer = setTimeout(() => { void run(); }, succeeded && latest && (latest.state === "draining" || latest.activeChains > 0) ? 3000 : 15000);
          }
          // Transient failures retry reads only; unsupported/auth waits for refresh or return.
        }
      }
    }
    const controls = { refresh: () => { void run(); }, command: (action: ContinuousMaintenanceAction) => { void run(action); } };
    operations.current = controls;
    function visibility() {
      clearTimeout(timer);
      if (document.visibilityState === "hidden") {
        confirmed = false; setAvailable(false);
        // An explicit command may already be accepted; only abort background reads here.
        if (inFlight && !inFlight.action) inFlight.controller.abort();
      } else controls.refresh();
    }
    controls.refresh(); document.addEventListener("visibilitychange", visibility);
    return () => {
      disposed = true; inFlight?.controller.abort(); clearTimeout(timer);
      document.removeEventListener("visibilitychange", visibility);
      if (operations.current === controls) operations.current = null;
    };
  }, [active, routeActive]);

  const visible = active && routeActive && document.visibilityState !== "hidden";
  const disabled = !visible || !available || loading || busy !== null;
  return <section aria-labelledby="continuous-maintenance-heading" className="grid gap-3">
    <SectionHeader id="continuous-maintenance-heading" title="Обслуживание непрерывных исследований" />
    <p className="text-sm text-muted-foreground">Для всех алгоритмов вашей учётной записи. Новые непрерывные цепочки временно не допускаются; уже допущенные завершают исследование, служебный replay, аналитику и допуск полностью.</p>
    <p className="text-sm text-muted-foreground">Это не Stop: действующие адаптивные профили не отключаются. Постоянный live, ручные расчёты, настройки и разрешения live не меняются.</p>
    {loading && <Spinner label="Обновление статуса обслуживания" />}
    {!snapshot && <p role="status" className="text-sm">Статус обслуживания неизвестен.</p>}
    {snapshot && <>
      {!available && <p role="status" className="text-sm">Показан последний подтверждённый снимок; актуальность статуса обслуживания не подтверждена.</p>}
      <div className="grid gap-2 text-sm">
        <div><Badge variant={!available ? "warning" : snapshot.state === "draining" ? "info" : "neutral"}>{labels[snapshot.state]}</Badge></div>
        <p>Незавершённых цепочек: {snapshot.activeChains}</p>
        <p>Цепочек с сохранённым снимком: {snapshot.resumableChains}</p>
        <p className="text-muted-foreground">Изменение флага обслуживания: {snapshot.updatedAt === null ? "флаг ещё не изменялся" : formatIsoDateTime(snapshot.updatedAt)}</p>
      </div>
    </>}
    <p className="text-xs text-muted-foreground">Счётчик сохранённых снимков учитывает только полностью зафиксированный вход. Это не гарантия совместимости будущей версии или полного восстановления поиска.</p>
    <p className="text-xs text-muted-foreground">Возобновление открывает только допуск: не запускает ранее остановленные процессы и не сбрасывает retries. Start во время обслуживания не обходит закрытый допуск. Завершение drain не разрешает очистку истории и само не обновляет сервер.</p>
    {readError && <Alert variant="destructive" role="alert">{readError}</Alert>}
    {commandError && <Alert variant="destructive" role="alert">{commandError}</Alert>}
    <div className="flex flex-wrap gap-2">
      <Button type="button" variant="outline" disabled={disabled || snapshot?.admissionPaused !== false} onClick={() => operations.current?.command("drain")}>Подготовить непрерывные исследования к обслуживанию</Button>
      <Button type="button" variant="outline" disabled={disabled || snapshot?.admissionPaused !== true} onClick={() => operations.current?.command("resume")}>Возобновить допуск непрерывных исследований</Button>
      <Button type="button" variant="outline" disabled={!visible || loading || busy !== null} onClick={() => operations.current?.refresh()}>Обновить статус обслуживания</Button>
    </div>
  </section>;
}
