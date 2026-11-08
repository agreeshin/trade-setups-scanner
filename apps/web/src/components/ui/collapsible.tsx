import * as React from "react";
import * as CollapsiblePrimitive from "@radix-ui/react-collapsible";
import { cn } from "../../lib/utils";

export const Collapsible = CollapsiblePrimitive.Root;

export function CollapsibleTrigger({ className, ...props }: React.ComponentProps<typeof CollapsiblePrimitive.Trigger>) {
  return <CollapsiblePrimitive.Trigger data-slot="collapsible-trigger" className={cn("flex cursor-pointer select-none items-center gap-1 font-medium transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50", className)} {...props} />;
}

export function CollapsibleContent({ className, ...props }: React.ComponentProps<typeof CollapsiblePrimitive.Content>) {
  return <CollapsiblePrimitive.Content data-slot="collapsible-content" className={cn("overflow-hidden focus-visible:outline-none", className)} {...props} />;
}
