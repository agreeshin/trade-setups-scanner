import type { HTMLAttributes } from "react";
import { cn } from "../../lib/utils";

export function Skeleton({ className, ...props }: HTMLAttributes<HTMLDivElement>) {
  return <div data-slot="skeleton" aria-hidden="true" className={cn("rounded-md bg-muted bg-linear-to-r from-muted via-muted-foreground/15 to-muted bg-size-[200%_100%] motion-safe:animate-skeleton-shimmer", className)} {...props} />;
}
