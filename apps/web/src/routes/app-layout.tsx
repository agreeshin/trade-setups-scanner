import { NavLink, Outlet } from "react-router";
import { Button } from "../components/ui/button";
import { cn } from "../lib/utils";
import { marketDataStatusChanged } from "../store/app-slice";
import { useAppDispatch, useAppSelector } from "../store/hooks";
export function AppLayout() {
  const dispatch = useAppDispatch();
  const marketDataStatus = useAppSelector((state) => state.app.marketDataStatus);
  return (
    <div className="flex min-h-screen flex-col">
      <header className="border-b border-input">
        <div className="mx-auto flex w-full max-w-5xl items-center justify-between px-4 py-3">
          <nav className="flex items-center gap-2" aria-label="Основная навигация">
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
          </nav>
          <button
            type="button"
            className={cn(
              "rounded-md px-2 py-1 text-xs",
              marketDataStatus === "ready"
                ? "bg-muted text-muted-foreground"
                : "bg-muted text-muted-foreground",
            )}
            onClick={() =>
              dispatch(
                marketDataStatusChanged(
                  marketDataStatus === "ready" ? "degraded" : "ready",
                ),
              )
            }
          >
            Сервис данных: {marketDataStatus === "ready" ? "готов" : "неизвестно"}
          </button>
        </div>
      </header>
      <main className="mx-auto w-full max-w-5xl flex-1 px-4 py-6">
        <Outlet />
      </main>
    </div>
  );
}
