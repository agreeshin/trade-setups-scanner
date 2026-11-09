import type { ButtonHTMLAttributes, HTMLAttributes } from "react";
import { cn } from "../../lib/utils";

type DivAttributes = HTMLAttributes<HTMLDivElement>;

export function SegmentedList({ className, ...props }: DivAttributes) {
  return <div data-slot="segmented-list" className={cn("inline-flex w-fit max-w-full flex-wrap items-center gap-1 rounded-lg border border-input bg-muted/40 p-1", className)} {...props} />;
}

export function SegmentedTrigger({ className, ...props }: ButtonHTMLAttributes<HTMLButtonElement>) {
  return <button type="button" data-slot="segmented-trigger" className={cn("inline-flex h-8 shrink-0 items-center justify-center whitespace-nowrap rounded-md px-3 text-sm font-medium text-muted-foreground transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:pointer-events-none disabled:opacity-50 aria-selected:bg-primary aria-selected:text-primary-foreground aria-selected:shadow-sm", className)} {...props} />;
}
