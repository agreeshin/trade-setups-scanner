import type { ReactNode } from "react";
import { ThemeSelect } from "./theme-select";

export function AppSurface({ children }: { children: ReactNode }) {
  return <div data-slot="app-surface" className="relative isolate min-h-dvh bg-background font-sans text-sm text-foreground antialiased">
    <div data-slot="ambient" aria-hidden="true" className="pointer-events-none fixed inset-0 -z-10 overflow-hidden opacity-(--opacity-ambient)">
      <div className="absolute -top-48 right-0 h-144 w-3/4 max-w-5xl rounded-full bg-radial from-ambient-rose/80 via-ambient-violet/70 to-transparent blur-[110px]" />
      <div className="absolute -bottom-64 -left-48 size-176 rounded-full bg-radial from-ambient-violet/80 via-ambient-rose/40 to-transparent blur-[110px]" />
    </div>
    {children}
  </div>;
}

export function SessionBar({ children }: { children: ReactNode }) {
  return <div className="flex min-h-12 flex-wrap items-center justify-between gap-3 border-b border-input/70 px-4 py-2 sm:px-6">
    <span className="flex items-center gap-2.5 font-semibold tracking-tight">
      <span aria-hidden="true" className="size-3.5 rounded-full border-2 border-primary" />
      Trade Setups
    </span>
    <div className="flex flex-wrap items-center gap-3"><ThemeSelect />{children}</div>
  </div>;
}

export function AppFrame({ navigation, status, children }: { navigation: ReactNode; status: ReactNode; children: ReactNode }) {
  return <div className="flex flex-col">
    <header className="sticky top-0 z-20 border-b border-input/60 bg-background/75 backdrop-blur-sm">
      <div className="mx-auto flex w-full max-w-7xl flex-wrap items-center justify-between gap-3 px-4 py-3 sm:px-6">
        <nav className="flex flex-wrap items-center gap-1" aria-label="Основная навигация">{navigation}</nav>
        <span className="rounded-full border border-input/60 bg-muted/40 px-3 py-1.5 text-xs text-muted-foreground">{status}</span>
      </div>
    </header>
    <main className="mx-auto w-full min-w-0 max-w-7xl flex-1 px-4 py-6 sm:px-6 sm:py-8">{children}</main>
  </div>;
}

export function AuthFrame({ children }: { children: ReactNode }) {
  return <main className="flex min-h-dvh flex-col items-center justify-center gap-4 p-6">
    <div className="flex w-full max-w-sm justify-end"><ThemeSelect /></div>
    {children}
  </main>;
}
