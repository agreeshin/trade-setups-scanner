import type { HTMLAttributes } from "react";
import { Slot } from "@radix-ui/react-slot";
import { cn } from "../../lib/utils";

export function Card({ asChild = false, className, ...props }: HTMLAttributes<HTMLDivElement> & { asChild?: boolean }) {
  const Comp = asChild ? Slot : "div";
  return <Comp data-slot="card" className={cn("rounded-md border border-input/80 bg-card/70 p-5 text-card-foreground", className)} {...props} />;
}
