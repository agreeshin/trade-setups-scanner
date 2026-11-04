import { NavLink, Outlet } from "react-router";
import { useEffect, useState } from "react";
import { Button } from "../components/ui/button";
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
  const refresh = () => { setStatus({ kind: "loading" }); setRevision((value) => value + 1); };
  return (
    <div className="flex min-h-screen flex-col">
      <header className="border-b border-input">
        <div className="mx-auto flex w-full max-w-5xl flex-wrap items-center justify-between gap-2 px-4 py-3">
          <nav className="flex flex-wrap items-center gap-2" aria-label="Основная навигация">
            <NavLink to="/" end>
              {({ isActive }) => (
                <Button variant={isActive ? "default" : "ghost"} size="sm">
                  Обзор
                </Button>
              )}
            </NavLink>
            <NavLink to="/setups">
              {({ isActive }) => (
                <Button variant={isActive ? "default" : "ghost"} size="sm">
                  Сетапы
                </Button>
              )}
            </NavLink>
            <NavLink to="/runs">
              {({ isActive }) => (
                <Button variant={isActive ? "default" : "ghost"} size="sm">
                  Запуски
                </Button>
              )}
            </NavLink>
            <NavLink to="/settings">
              {({ isActive }) => (
                <Button variant={isActive ? "default" : "ghost"} size="sm">
                  Настройки
                </Button>
              )}
            </NavLink>
            <NavLink to="/history">{({ isActive }) => <Button variant={isActive ? "default" : "ghost"} size="sm">История и сбор</Button>}</NavLink>
          </nav>
          <span className="rounded-md bg-muted px-2 py-1 text-xs text-muted-foreground">
            Сервис данных: {status.kind === "ready" ? marketDataLabel(status.data.marketData) : status.kind === "loading" ? "проверка…" : "статус недоступен"}
          </span>
        </div>
      </header>
      <main className="mx-auto w-full max-w-5xl flex-1 px-4 py-6">
        <Outlet context={{ status, refresh } satisfies StatusContext} />
      </main>
    </div>
  );
}
