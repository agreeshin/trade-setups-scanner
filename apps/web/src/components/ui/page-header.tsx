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
    <header data-slot="page-header" className={cn("grid w-full min-w-0 gap-3", className)}>
      <div className="grid gap-1.5">
        <div className="flex min-w-0 items-center justify-center gap-2 text-center"><h1 id={id} className="min-w-0 text-2xl font-semibold tracking-tight">{title}</h1>{description && <InfoNote label="О странице">{description}</InfoNote>}</div>
      </div>
      {actions ? <div className="flex flex-wrap items-center gap-2">{actions}</div> : null}
    </header>
  );
}

export function SectionHeader({ title, description, actions, className, id }: HeaderProps) {
  return (
    <div data-slot="section-header" className={cn("grid min-w-0 flex-1 gap-2", className)}>
      <div className="grid gap-1">
        <div className="flex min-w-0 items-center justify-center gap-2 text-center"><h2 id={id} className="min-w-0 text-lg font-semibold tracking-tight">{title}</h2>{description && <InfoNote label="О разделе">{description}</InfoNote>}</div>
      </div>
      {actions ? <div className="flex flex-wrap items-center gap-2">{actions}</div> : null}
    </div>
  );
}
