import type { SelectHTMLAttributes } from "react";
import { ChevronDown } from "lucide-react";
import { cn } from "../../lib/utils";

export function Select({ className, ...props }: SelectHTMLAttributes<HTMLSelectElement>) {
  return <span data-slot="select-wrapper" className="relative inline-flex min-w-0">
    <select data-slot="select" className={cn("h-9 min-w-0 w-full appearance-none rounded-lg border border-input bg-background py-1 pl-3 pr-9 text-sm shadow-sm transition-colors hover:border-muted-foreground/50 hover:bg-accent/40 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50", className)} {...props} />
    {!props.multiple && <ChevronDown aria-hidden="true" className="pointer-events-none absolute right-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />}
  </span>;
}
