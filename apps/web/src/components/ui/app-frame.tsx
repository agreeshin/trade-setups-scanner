import { createContext, useContext, type ReactNode } from "react";
import { ThemeSelect } from "./theme-select";

export function AppSurface({ children }: { children: ReactNode }) {
  return <div data-slot="app-surface" className="relative isolate min-h-dvh bg-background font-sans text-sm text-foreground antialiased">
    {children}
  </div>;
}

const SessionSlotContext = createContext<ReactNode>(null);

export function SessionSlotProvider({ value, children }: { value: ReactNode; children: ReactNode }) {
  return <SessionSlotContext.Provider value={value}>{children}</SessionSlotContext.Provider>;
}

export function SessionSlot() {
  return <>{useContext(SessionSlotContext)}</>;
}

export function AppFrame({ navigation, status, children }: { navigation: ReactNode; status: ReactNode; children: ReactNode }) {
  return <div className="flex flex-col">
    <header className="sticky top-0 z-20 border-b border-input/60 bg-background">
      <div className="mx-auto flex w-full max-w-7xl flex-wrap items-center gap-x-3 gap-y-2 px-4 py-2.5 sm:px-6">
        <span className="flex items-center gap-2.5 font-semibold tracking-tight">
          <span aria-hidden="true" className="size-3.5 rounded-full border-2 border-primary" />
          Trade Setups
        </span>
        <nav className="flex flex-wrap items-center gap-1" aria-label="Основная навигация">{navigation}</nav>
        <div className="ml-auto flex flex-wrap items-center gap-3">
          <span className="rounded-full border border-input/60 bg-muted/40 px-3 py-1.5 text-xs text-muted-foreground">{status}</span>
          <ThemeSelect />
          <SessionSlot />
        </div>
      </div>
    </header>
    <main className="mx-auto w-full max-w-7xl flex-1 px-4 py-6 sm:px-6 sm:py-8">
      <div data-slot="page-card" className="rounded-xl border border-input/60 bg-card p-5 sm:p-8">{children}</div>
    </main>
  </div>;
}

export function AuthFrame({ children }: { children: ReactNode }) {
  return <main className="flex min-h-dvh flex-col items-center justify-center gap-4 p-6">
    <div className="flex w-full max-w-sm justify-end"><ThemeSelect /></div>
    {children}
  </main>;
}
