import * as React from "react";
import { cn } from "../../lib/utils";

export type InputProps = React.InputHTMLAttributes<HTMLInputElement>;

export function Input({ className, type, ...props }: InputProps) {
  return (
    <input
      data-slot="input"
      type={type}
      className={cn(
        "flex h-9 min-w-0 w-full rounded-lg border border-input bg-background px-3 py-1 text-sm tabular-nums accent-primary transition-colors placeholder:text-muted-foreground/70 hover:border-muted-foreground/50 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50 file:mr-3 file:cursor-pointer file:rounded-md file:border-0 file:bg-muted file:px-3 file:py-1.5 file:text-xs file:font-medium file:text-foreground hover:file:bg-accent",
        className,
      )}
      {...props}
    />
  );
}
