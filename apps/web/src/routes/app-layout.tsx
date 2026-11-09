import { NavLink, Outlet } from "react-router";
import { useEffect, useState } from "react";
import { Button } from "../components/ui/button";
import { AppFrame } from "../components/ui/app-frame";
import { applyInterfaceSettings } from "../lib/datetime-format";
import { fetchPlatform, type PlatformState } from "../lib/platform-api";

export interface ServiceStatus { platform: "ready"; marketData: "ready" | "not_ready" | "unreachable" }
export interface StatusContext { status: PlatformState<ServiceStatus>; refresh: () => void }
export const marketDataLabel = (value: ServiceStatus["marketData"]): string => ({ ready: "готов", not_ready: "не готов", unreachable: "недоступен" })[value];

export function AppLayout() {
  const [status, setStatus] = useState<PlatformState<ServiceStatus>>({ kind: "loading" });
  const [revision, setRevision] = useState(0);
  useEffect(() => {
    let cancelled = false;
    void fetchPlatform<ServiceStatus>("/v1/status").then((result) => { if (!cancelled) setStatus(result); });
    return () => { cancelled = true; };
  }, [revision]);
  // Sync persisted interface date/time settings from the platform once per layout mount.
  useEffect(() => {
    let cancelled = false;
    void fetchPlatform<{ value: { interface?: unknown } }>("/v1/settings/ui").then((result) => {
      if (!cancelled && result.kind === "ready" && result.data.value.interface !== undefined) applyInterfaceSettings(result.data.value.interface);
    });
    return () => { cancelled = true; };
  }, []);
  const refresh = () => { setStatus({ kind: "loading" }); setRevision((value) => value + 1); };
  return (
    <AppFrame navigation={<>
      <Button asChild variant="navigation" size="sm"><NavLink to="/" end>Обзор</NavLink></Button>
      <Button asChild variant="navigation" size="sm"><NavLink to="/setups" end>Сетапы</NavLink></Button>
      <Button asChild variant="navigation" size="sm"><NavLink to="/analytics" end>Аналитика</NavLink></Button>
      <Button asChild variant="navigation" size="sm"><NavLink to="/runs">Запуски</NavLink></Button>
      <Button asChild variant="navigation" size="sm"><NavLink to="/settings">Настройки</NavLink></Button>
      <Button asChild variant="navigation" size="sm"><NavLink to="/history">История и сбор</NavLink></Button>
    </>} status={<>Сервис данных: {status.kind === "ready" ? marketDataLabel(status.data.marketData) : status.kind === "loading" ? "проверка…" : "статус недоступен"}</>}>
      <Outlet context={{ status, refresh } satisfies StatusContext} />
    </AppFrame>
  );
}
