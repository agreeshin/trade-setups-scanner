import type { ReactNode } from "react";
import { cn } from "../../lib/utils";
import { InfoNote } from "./info-note";

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
        <div className="flex items-center gap-2"><h1 id={id} className="text-2xl font-semibold tracking-tight">{title}</h1>{description && <InfoNote label="О странице">{description}</InfoNote>}</div>
      </div>
      {actions ? <div className="flex flex-wrap items-center gap-2">{actions}</div> : null}
    </header>
  );
}

export function SectionHeader({ title, description, actions, className, id }: HeaderProps) {
  return (
    <div data-slot="section-header" className={cn("flex flex-wrap items-end justify-between gap-x-4 gap-y-2", className)}>
      <div className="grid gap-1">
        <div className="flex items-center gap-2"><h2 id={id} className="text-lg font-semibold tracking-tight">{title}</h2>{description && <InfoNote label="О разделе">{description}</InfoNote>}</div>
      </div>
      {actions ? <div className="flex flex-wrap items-center gap-2">{actions}</div> : null}
    </div>
  );
}
