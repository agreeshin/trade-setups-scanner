import { useState } from "react";
import { RefreshCw } from "lucide-react";
import { Alert, AlertDescription } from "../components/ui/alert";
import { IconButton } from "../components/ui/icon-button";
import { Spinner } from "../components/ui/spinner";
import { useApplicationLoad } from "./app-layout";
import { localTime } from "./platform-controls";
import { useRouteActive } from "./route-active";
import { SetupsAnalyticsPanel } from "./setups-analytics-panel";
import { useSetupOutcomesRefresh } from "./use-setup-outcomes-refresh";

export function SetupsPage() {
  const active = useRouteActive();
  const application = useApplicationLoad();
  const [mountedAt] = useState(() => Math.floor(Date.now() / 1000));
  const loadedAtUnix = application?.loadedAtUnix ?? mountedAt;
  const interfaceReady = application?.interfaceReady ?? true;
  const outcomes = useSetupOutcomesRefresh(active && interfaceReady);
  return <section aria-label="Сетапы" className="grid min-w-0 gap-4">
    {outcomes.pending && <div role="status" className="flex items-center gap-2 text-sm text-muted-foreground"><Spinner label="Обновление результатов сделок" /><span>Обновление результатов: обработано {outcomes.processed}, рассчитано {outcomes.computed}, обновлено {outcomes.updated}…</span></div>}
    {outcomes.error && <Alert variant="destructive" role="alert"><AlertDescription>Обновление результатов прервано: {outcomes.error} Сохранённые результаты не удалены; повторите обновление вручную.</AlertDescription></Alert>}
    {!outcomes.pending && outcomes.completedAt !== null && <p role="status" className="text-xs text-muted-foreground">Результаты обновлены: {localTime(outcomes.completedAt)} · обработано {outcomes.processed}, обновлено {outcomes.updated}.</p>}
    <SetupsAnalyticsPanel initialLoad={{ loadedAtUnix, interfaceReady, ready: outcomes.completedAt !== null && !outcomes.pending && outcomes.error === null }}
      outcomesRevision={outcomes.revision}
      refreshControl={<IconButton label="Обновить результаты сетапов" icon={RefreshCw} disabled={outcomes.pending || !interfaceReady} aria-busy={outcomes.pending} onClick={() => { void outcomes.refresh(); }} />} />
  </section>;
}
