import { useId, type ReactNode } from "react";
import { cn } from "../../lib/utils";

export interface FieldControlProps {
  id: string;
  "aria-describedby": string | undefined;
  "aria-invalid": true | undefined;
}

export interface FieldProps {
  label: ReactNode;
  children: (control: FieldControlProps) => ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  className?: string;
}

export function Field({ label, children, hint, error, className }: FieldProps) {
  const controlId = useId();
  const hintId = `${controlId}-hint`;
  const errorId = `${controlId}-error`;
  const describedBy = [hint ? hintId : null, error ? errorId : null].filter(Boolean).join(" ") || undefined;
  return (
    <div data-slot="field" className={cn("grid gap-1.5 text-sm", className)}>
      <label htmlFor={controlId} className="font-medium leading-none">{label}</label>
      {children({ id: controlId, "aria-describedby": describedBy, "aria-invalid": error ? true : undefined })}
      {hint ? <p id={hintId} className="text-xs text-muted-foreground">{hint}</p> : null}
      {error ? <p id={errorId} role="alert" className="text-xs text-danger">{error}</p> : null}
    </div>
  );
}
