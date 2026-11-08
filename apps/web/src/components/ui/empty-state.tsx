import type { HTMLAttributes, ReactNode } from "react";
import { cn } from "../../lib/utils";

export interface EmptyStateProps extends Omit<HTMLAttributes<HTMLDivElement>, "title"> {
  title: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
}

export function EmptyState({ title, description, actions, className, ...props }: EmptyStateProps) {
  return (
    <div data-slot="empty-state" className={cn("grid place-items-center gap-2 rounded-lg border border-input/60 bg-card/60 px-6 py-10 text-center", className)} {...props}>
      <p className="font-medium">{title}</p>
      {description ? <p className="max-w-md text-sm text-muted-foreground">{description}</p> : null}
      {actions ? <div className="mt-2 flex flex-wrap justify-center gap-2">{actions}</div> : null}
    </div>
  );
}
