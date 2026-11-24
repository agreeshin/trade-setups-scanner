import * as PopoverPrimitive from "@radix-ui/react-popover";
import {
  Children,
  Fragment,
  forwardRef,
  isValidElement,
  useEffect,
  useId,
  useRef,
  useState,
  type ChangeEvent,
  type KeyboardEvent,
  type ReactNode,
  type SelectHTMLAttributes,
} from "react";
import { Check, ChevronDown, Search } from "lucide-react";
import { cn } from "../../lib/utils";

interface SelectOption {
  disabled: boolean;
  label: ReactNode;
  searchLabel: string;
  value: string;
}

export interface SelectProps extends SelectHTMLAttributes<HTMLSelectElement> {
  searchable?: boolean;
  searchPlaceholder?: string;
  emptyText?: string;
}

function textContent(value: ReactNode): string {
  return Children.toArray(value).map((child) => {
    if (typeof child === "string" || typeof child === "number") return String(child);
    return isValidElement<{ children?: ReactNode }>(child) ? textContent(child.props.children) : "";
  }).join("");
}

function readOptions(children: ReactNode): SelectOption[] {
  const options: SelectOption[] = [];
  const visit = (nodes: ReactNode, groupDisabled = false) => Children.forEach(nodes, (child) => {
    if (!isValidElement<{ children?: ReactNode; disabled?: boolean; value?: string | number }>(child)) return;
    if (child.type === Fragment || child.type === "optgroup") {
      visit(child.props.children, groupDisabled || child.props.disabled === true);
      return;
    }
    if (child.type !== "option") return;
    const searchLabel = textContent(child.props.children);
    options.push({
      disabled: groupDisabled || child.props.disabled === true,
      label: child.props.children,
      searchLabel,
      value: String(child.props.value ?? searchLabel),
    });
  });
  visit(children);
  return options;
}

function initialValue(value: SelectProps["value"], defaultValue: SelectProps["defaultValue"], options: SelectOption[]) {
  const candidate = value ?? defaultValue;
  if (candidate !== undefined && !Array.isArray(candidate)) return String(candidate);
  return options.find((option) => !option.disabled)?.value ?? "";
}

function NativeSelect(props: SelectProps & { ref?: React.Ref<HTMLSelectElement> }) {
  const { className } = props;
  const nativeProps = { ...props };
  delete nativeProps.searchable;
  delete nativeProps.searchPlaceholder;
  delete nativeProps.emptyText;
  delete nativeProps.className;
  return <span data-slot="select-wrapper" className="relative inline-flex min-w-0">
    <select data-slot="select" className={cn("h-9 min-w-0 w-full appearance-none rounded-lg border border-input bg-background py-1 pl-3 pr-9 text-sm shadow-sm transition-colors hover:border-muted-foreground/50 hover:bg-accent/40 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50", className)} {...nativeProps} />
    {!props.multiple && <ChevronDown aria-hidden="true" className="pointer-events-none absolute right-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />}
  </span>;
}

function CustomSelect({
  children,
  className,
  defaultValue,
  disabled,
  emptyText = "Ничего не найдено",
  onChange,
  searchable = false,
  searchPlaceholder = "Найти…",
  value,
  ref,
  ...props
}: SelectProps & { ref?: React.Ref<HTMLSelectElement> }) {
  const options = readOptions(children);
  const generatedId = useId();
  const selectId = props.id ?? generatedId;
  const listId = `${selectId}-options`;
  const nativeRef = useRef<HTMLSelectElement | null>(null);
  const searchRef = useRef<HTMLInputElement | null>(null);
  const listRef = useRef<HTMLDivElement | null>(null);
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [uncontrolledValue, setUncontrolledValue] = useState(() => initialValue(value, defaultValue, options));
  const selectedValue = value !== undefined && !Array.isArray(value)
    ? String(value)
    : options.some((option) => option.value === uncontrolledValue)
      ? uncontrolledValue
      : options.find((option) => !option.disabled)?.value ?? "";
  const normalizedQuery = query.trim().toLocaleLowerCase();
  const filtered = searchable && normalizedQuery
    ? options.filter((option) => option.searchLabel.toLocaleLowerCase().includes(normalizedQuery))
    : options;
  const enabled = filtered.filter((option) => !option.disabled);
  const selected = options.find((option) => option.value === selectedValue);
  const [activeValue, setActiveValue] = useState(selectedValue);
  useEffect(() => {
    if (open) listRef.current?.querySelector('[data-active="true"]')?.scrollIntoView?.({ block: "nearest" });
  }, [open, activeValue]);

  const assignRef = (node: HTMLSelectElement | null) => {
    nativeRef.current = node;
    if (typeof ref === "function") ref(node);
    else if (ref) ref.current = node;
  };

  useEffect(() => {
    const select = nativeRef.current;
    const form = select?.form;
    if (!select || !form) return;
    const syncAfterReset = () => queueMicrotask(() => setUncontrolledValue(select.value));
    form.addEventListener("reset", syncAfterReset);
    return () => form.removeEventListener("reset", syncAfterReset);
  }, []);

  const handleNativeChange = (event: ChangeEvent<HTMLSelectElement>) => {
    setUncontrolledValue(event.currentTarget.value);
    setActiveValue(event.currentTarget.value);
    onChange?.(event);
  };

  const commit = (next: string) => {
    const select = nativeRef.current;
    if (!select || select.value === next) {
      setOpen(false);
      return;
    }
    select.value = next;
    select.dispatchEvent(new Event("input", { bubbles: true }));
    select.dispatchEvent(new Event("change", { bubbles: true }));
    setOpen(false);
  };

  const moveActive = (direction: 1 | -1) => {
    if (enabled.length === 0) return;
    const index = enabled.findIndex((option) => option.value === activeValue);
    const next = index < 0 ? (direction === 1 ? 0 : enabled.length - 1) : (index + direction + enabled.length) % enabled.length;
    setActiveValue(enabled[next]!.value);
  };

  const handleListKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      moveActive(event.key === "ArrowDown" ? 1 : -1);
    } else if (event.key === "Home" || event.key === "End") {
      event.preventDefault();
      const next = event.key === "Home" ? enabled[0] : enabled.at(-1);
      if (next) setActiveValue(next.value);
    } else if (event.key === "Enter") {
      event.preventDefault();
      if (enabled.some((option) => option.value === activeValue)) commit(activeValue);
    } else if (event.key === "Escape") {
      event.preventDefault();
      setOpen(false);
    }
  };

  const changeOpen = (next: boolean) => {
    setOpen(next);
    setQuery("");
    setActiveValue(selectedValue);
  };

  return <PopoverPrimitive.Root open={open} onOpenChange={changeOpen}>
    <span data-slot="select-wrapper" className="relative inline-flex min-w-0 focus-within:rounded-lg focus-within:ring-2 focus-within:ring-ring">
      <select
        {...props}
        ref={assignRef}
        data-slot="select"
        className="pointer-events-none absolute inset-0 h-full w-full opacity-0"
        defaultValue={defaultValue}
        disabled={disabled}
        onChange={handleNativeChange}
        aria-expanded={open}
        aria-controls={open ? listId : undefined}
        onKeyDown={(event) => {
          props.onKeyDown?.(event);
          if (!event.defaultPrevented && ["Enter", " ", "ArrowDown", "ArrowUp"].includes(event.key)) { event.preventDefault(); changeOpen(true); }
        }}
        value={value}
      >{options.map((option) => <option key={option.value} value={option.value} disabled={option.disabled} label={option.searchLabel} aria-label={option.searchLabel} />)}</select>
      <PopoverPrimitive.Trigger asChild>
        <button
          type="button"
          tabIndex={-1}
          aria-hidden="true"
          data-slot="select-trigger"
          disabled={disabled}
          className={cn("flex h-9 min-w-0 w-full items-center justify-between gap-2 rounded-lg border border-input bg-background py-1 pl-3 pr-3 text-left text-sm shadow-sm transition-colors hover:border-muted-foreground/50 hover:bg-accent/40 disabled:cursor-not-allowed disabled:opacity-50", className)}
        >
          <span data-slot="select-value" className="min-w-0 flex-1 truncate">{selected?.label ?? ""}</span>
          <ChevronDown className={cn("size-4 shrink-0 text-muted-foreground transition-transform", open && "rotate-180")} />
        </button>
      </PopoverPrimitive.Trigger>
    </span>
    <PopoverPrimitive.Portal>
      <PopoverPrimitive.Content
        data-slot="select-content"
        sideOffset={6}
        align="start"
        collisionPadding={12}
        onCloseAutoFocus={(event) => { event.preventDefault(); nativeRef.current?.focus(); }}
        className="z-50 w-[var(--radix-popover-trigger-width)] min-w-48 overflow-hidden rounded-xl border border-input bg-card text-card-foreground shadow-card outline-none"
        onOpenAutoFocus={(event) => {
          event.preventDefault();
          queueMicrotask(() => (searchable ? searchRef.current : listRef.current)?.focus());
        }}
      >
        {searchable && <label className="m-2 flex items-center gap-2 rounded-lg border border-input bg-background px-2.5 focus-within:ring-2 focus-within:ring-ring">
          <Search aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
          <span className="sr-only">Поиск по вариантам</span>
          <input
            type="search"
            ref={searchRef}
            data-slot="select-search"
            value={query}
            aria-controls={listId}
            aria-activedescendant={enabled.some((option) => option.value === activeValue) ? `${selectId}-option-${encodeURIComponent(activeValue)}` : undefined}
            placeholder={searchPlaceholder}
            className="h-8 min-w-0 flex-1 bg-transparent text-sm outline-none placeholder:text-muted-foreground/70"
            onChange={(event) => {
              const next = event.target.value;
              setQuery(next);
              const first = options.find((option) => !option.disabled && option.searchLabel.toLocaleLowerCase().includes(next.trim().toLocaleLowerCase()));
              setActiveValue(first?.value ?? "");
            }}
            onKeyDown={handleListKeyDown}
          />
        </label>}
        <div
          ref={listRef}
          id={listId}
          role="listbox"
          aria-label={props["aria-label"] ?? "Варианты"}
          tabIndex={searchable ? -1 : 0}
          aria-activedescendant={enabled.some((option) => option.value === activeValue) ? `${selectId}-option-${encodeURIComponent(activeValue)}` : undefined}
          className="max-h-72 overflow-y-auto p-1.5 outline-none"
          onKeyDown={handleListKeyDown}
        >
          {filtered.map((option) => {
            const isSelected = option.value === selectedValue;
            const isActive = option.value === activeValue;
            return <button
              key={option.value}
              id={`${selectId}-option-${encodeURIComponent(option.value)}`}
              type="button"
              tabIndex={-1}
              role="option"
              aria-selected={isSelected}
              disabled={option.disabled}
              data-active={isActive ? "true" : undefined}
              data-selected={isSelected ? "true" : undefined}
              className="flex w-full items-center gap-2 rounded-lg px-2.5 py-2 text-left text-sm outline-none transition-colors hover:bg-ambient-violet/15 focus-visible:bg-ambient-violet/15 disabled:pointer-events-none disabled:opacity-45 data-[active=true]:bg-ambient-violet/15 data-[selected=true]:bg-ambient-violet/25"
              onPointerMove={() => { if (!option.disabled) setActiveValue(option.value); }}
              onClick={() => commit(option.value)}
            >
              <span className="min-w-0 flex-1 truncate">{option.label}</span>
              <Check aria-hidden="true" className={cn("size-4 shrink-0 text-ambient-violet", isSelected ? "opacity-100" : "opacity-0")} />
            </button>;
          })}
          {filtered.length === 0 && <p data-slot="select-empty" className="px-3 py-6 text-center text-sm text-muted-foreground">{emptyText}</p>}
        </div>
      </PopoverPrimitive.Content>
    </PopoverPrimitive.Portal>
  </PopoverPrimitive.Root>;
}

export const Select = forwardRef<HTMLSelectElement, SelectProps>(function Select(props, ref) {
  if (props.multiple || (typeof props.size === "number" && props.size > 1)) return <NativeSelect {...props} ref={ref} />;
  return <CustomSelect {...props} ref={ref} />;
});
