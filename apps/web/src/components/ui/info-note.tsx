import type { ReactNode } from "react";
import { cn } from "../../lib/utils";
import { Tooltip } from "./tooltip";

export function InfoNote({ children, label = "Пояснение", className }: { children: ReactNode; label?: string; className?: string }) {
  return <span data-slot="info-note" className={cn("inline-flex items-center", className)}>
    <Tooltip content={children}><button type="button" aria-label={label} className="inline-flex size-7 shrink-0 cursor-help items-center justify-center rounded-full border-0 bg-muted/60 text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"><span aria-hidden="true" className="font-serif text-base font-semibold leading-none">i</span></button></Tooltip>
  </span>;
}
