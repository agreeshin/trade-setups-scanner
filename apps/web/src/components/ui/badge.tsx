import type { HTMLAttributes } from "react";
import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "../../lib/utils";

export const badgeVariants = cva(
  "inline-flex w-fit shrink-0 items-center justify-center gap-1 whitespace-nowrap rounded-full px-2.5 py-0.5 text-xs font-medium",
  {
    variants: {
      variant: {
        neutral: "bg-muted/60 text-muted-foreground",
        success: "bg-success/12 text-success",
        warning: "bg-warning/12 text-warning",
        danger: "bg-danger/12 text-danger",
        info: "bg-info/12 text-info",
      },
    },
    defaultVariants: { variant: "neutral" },
  },
);

export type BadgeVariant = NonNullable<VariantProps<typeof badgeVariants>["variant"]>;

export interface BadgeProps extends HTMLAttributes<HTMLSpanElement>, VariantProps<typeof badgeVariants> {}

export function Badge({ className, variant, ...props }: BadgeProps) {
  return <span data-slot="badge" className={cn(badgeVariants({ variant }), className)} {...props} />;
}

const RUN_STATUS_VARIANTS: Record<string, BadgeVariant> = {
  queued: "neutral",
  running: "info",
  cancel_requested: "warning",
  done: "success",
  failed: "danger",
  cancelled: "neutral",
};

const SERVICE_STATUS_VARIANTS: Record<string, BadgeVariant> = {
  loading: "neutral",
  ready: "success",
  not_ready: "warning",
  unreachable: "danger",
};

export function runStatusVariant(status: string): BadgeVariant {
  return RUN_STATUS_VARIANTS[status] ?? "neutral";
}

export function serviceStatusVariant(status: string): BadgeVariant {
  return SERVICE_STATUS_VARIANTS[status] ?? "neutral";
}
