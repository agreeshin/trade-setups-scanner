import type { ReactNode } from "react";
import { cn } from "../../lib/utils";

interface HeaderProps {
  title: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  className?: string;
  id?: string;
}

export function PageHeader({ title, description, actions, className, id }: HeaderProps) {
  return (
    <header data-slot="page-header" className={cn("flex flex-wrap items-end justify-between gap-x-4 gap-y-3", className)}>
      <div className="grid gap-1.5">
        <h1 id={id} className="text-2xl font-semibold tracking-tight">{title}</h1>
        {description ? <p className="max-w-3xl text-sm text-muted-foreground">{description}</p> : null}
      </div>
      {actions ? <div className="flex flex-wrap items-center gap-2">{actions}</div> : null}
    </header>
  );
}

export function SectionHeader({ title, description, actions, className, id }: HeaderProps) {
  return (
    <div data-slot="section-header" className={cn("flex flex-wrap items-end justify-between gap-x-4 gap-y-2", className)}>
      <div className="grid gap-1">
        <h2 id={id} className="text-lg font-semibold tracking-tight">{title}</h2>
        {description ? <p className="max-w-3xl text-sm text-muted-foreground">{description}</p> : null}
      </div>
      {actions ? <div className="flex flex-wrap items-center gap-2">{actions}</div> : null}
    </div>
  );
}
