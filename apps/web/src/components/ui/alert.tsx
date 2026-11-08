import type { HTMLAttributes } from "react";
import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "../../lib/utils";

const alertVariants = cva("grid w-full gap-1.5 rounded-lg border px-4 py-3 text-sm", {
  variants: {
    variant: {
      info: "border-info/35 bg-info/10",
      success: "border-success/35 bg-success/10",
      warning: "border-warning/35 bg-warning/10",
      destructive: "border-danger/40 bg-danger/10",
    },
  },
  defaultVariants: { variant: "info" },
});

export interface AlertProps extends HTMLAttributes<HTMLDivElement>, VariantProps<typeof alertVariants> {}

export function Alert({ className, variant, ...props }: AlertProps) {
  return <div data-slot="alert" className={cn(alertVariants({ variant }), className)} {...props} />;
}

export function AlertTitle({ className, ...props }: HTMLAttributes<HTMLParagraphElement>) {
  return <p data-slot="alert-title" className={cn("font-medium leading-tight", className)} {...props} />;
}

export function AlertDescription({ className, ...props }: HTMLAttributes<HTMLParagraphElement>) {
  return <p data-slot="alert-description" className={cn("text-sm text-muted-foreground", className)} {...props} />;
}
