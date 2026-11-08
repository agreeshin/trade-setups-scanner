import type { HTMLAttributes } from "react";
import { cn } from "../../lib/utils";

export interface SeparatorProps extends HTMLAttributes<HTMLHRElement> {
  orientation?: "horizontal" | "vertical";
}

export function Separator({ className, orientation = "horizontal", ...props }: SeparatorProps) {
  return <hr data-slot="separator" className={cn("border-0 bg-input/70", orientation === "horizontal" ? "h-px w-full" : "h-full w-px self-stretch", className)} {...props} />;
}
