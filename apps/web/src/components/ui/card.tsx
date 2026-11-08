import type { HTMLAttributes } from "react";
import { Slot } from "@radix-ui/react-slot";
import { cn } from "../../lib/utils";

type DivAttributes = HTMLAttributes<HTMLDivElement>;

export function Card({ asChild = false, className, ...props }: DivAttributes & { asChild?: boolean }) {
  const Comp = asChild ? Slot : "div";
  return <Comp data-slot="card" className={cn("rounded-xl border border-input/60 bg-muted/40 p-5 text-card-foreground", className)} {...props} />;
}

export function CardHeader({ className, ...props }: DivAttributes) {
  return <div data-slot="card-header" className={cn("flex flex-col gap-1.5", className)} {...props} />;
}

export function CardTitle({ className, ...props }: HTMLAttributes<HTMLHeadingElement>) {
  return <h3 data-slot="card-title" className={cn("text-base font-semibold leading-tight tracking-tight", className)} {...props} />;
}

export function CardDescription({ className, ...props }: DivAttributes) {
  return <p data-slot="card-description" className={cn("text-sm text-muted-foreground", className)} {...props} />;
}

export function CardContent({ className, ...props }: DivAttributes) {
  return <div data-slot="card-content" className={cn("grid gap-2 text-sm", className)} {...props} />;
}

export function CardFooter({ className, ...props }: DivAttributes) {
  return <div data-slot="card-footer" className={cn("flex flex-wrap items-center gap-2", className)} {...props} />;
}
