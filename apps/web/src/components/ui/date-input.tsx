import { useState, useSyncExternalStore, type InputHTMLAttributes } from "react";
import { CalendarDays } from "lucide-react";
import { calendarInputPlaceholder, formatCalendarInput, interfaceSettings, subscribeInterfaceSettings } from "../../lib/datetime-format";
import { cn } from "../../lib/utils";

/** Keep native calendar/keyboard/validation and canonical ISO events; format only the visible value. */
export function DateInput({ className, onChange, onClick, ...props }: InputHTMLAttributes<HTMLInputElement>) {
  useSyncExternalStore(subscribeInterfaceSettings, interfaceSettings, interfaceSettings);
  const [uncontrolled, setUncontrolled] = useState(() => String(props.defaultValue ?? ""));
  const value = String(props.value ?? uncontrolled);
  return <span data-slot="date-control" className={cn(className, "relative focus-within:ring-2 focus-within:ring-ring has-[:disabled]:cursor-not-allowed has-[:disabled]:opacity-50")}>
    <span data-slot="date-display" aria-hidden="true" className={cn("pointer-events-none block min-w-0 truncate pr-7", value === "" && "text-muted-foreground/70")}>{value ? formatCalendarInput(value) : props.placeholder ?? calendarInputPlaceholder(props.type === "datetime-local")}</span>
    <CalendarDays aria-hidden="true" className="pointer-events-none absolute right-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
    <input {...props} data-slot="input" className="absolute inset-0 size-full cursor-pointer opacity-0 disabled:cursor-not-allowed [&::-webkit-calendar-picker-indicator]:absolute [&::-webkit-calendar-picker-indicator]:right-0 [&::-webkit-calendar-picker-indicator]:size-9" onChange={(event) => {
      if (props.value === undefined) setUncontrolled(event.target.value);
      onChange?.(event);
    }} onClick={(event) => {
      onClick?.(event);
      if (!event.defaultPrevented && !props.disabled && !props.readOnly && typeof event.currentTarget.showPicker === "function") {
        event.preventDefault();
        event.currentTarget.showPicker();
      }
    }} />
  </span>;
}
