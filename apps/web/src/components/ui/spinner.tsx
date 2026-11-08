import type { HTMLAttributes } from "react";
import { cn } from "../../lib/utils";

export interface SpinnerProps extends HTMLAttributes<HTMLSpanElement> {
  label: string;
}

export function Spinner({ label, className, ...props }: SpinnerProps) {
  return (
    <span {...props} role="status" aria-label={label} data-slot="spinner" className={cn("inline-flex items-center gap-2 text-sm text-muted-foreground", className)}>
      <svg aria-hidden="true" viewBox="0 0 24 24" fill="none" className="size-4 animate-spin">
        <circle cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" className="opacity-25" />
        <path d="M12 2a10 10 0 0 1 10 10" stroke="currentColor" strokeWidth="4" strokeLinecap="round" className="opacity-90" />
      </svg>
    </span>
  );
}
