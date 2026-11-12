import { createContext, useContext, useId, useState, type ReactNode } from "react";
import { Button } from "./button";

const SidebarContext = createContext<{ id: string; selected: string; visited: ReadonlySet<string> } | null>(null);

interface SidebarItem<T extends string> { id: T; label: string; status?: string }
interface SidebarVisitState { selected: string; visited: ReadonlySet<string> }

/** Navigation precedes content on mobile; the same retained panels sit to its left on desktop. */
export function PageSidebar<T extends string>({ label, items, value, onValueChange, children }: {
  label: string; items: readonly SidebarItem<T>[]; value: T;
  onValueChange: (value: T) => void; children: ReactNode;
}) {
  const id = useId();
  const [visitState, setVisitState] = useState<SidebarVisitState>(() => ({ selected: value, visited: new Set([value]) }));
  let acceptedVisitState = visitState;
  if (visitState.selected !== value) {
    acceptedVisitState = { selected: value, visited: new Set([...visitState.visited, value]) };
    setVisitState(acceptedVisitState);
  }
  return <SidebarContext.Provider value={{ id, selected: value, visited: acceptedVisitState.visited }}>
    <div data-slot="page-sidebar-layout" className={items.length ? "grid min-w-0 gap-6 md:grid-cols-[minmax(0,1fr)_14rem]" : "min-w-0"}>
      {items.length > 0 && <aside data-slot="page-sidebar" className="min-w-0 border-b border-input/60 pb-4 md:col-start-2 md:row-start-1 md:border-b-0 md:border-l md:pb-0 md:pl-5">
        <div role="tablist" aria-label={label} aria-orientation="vertical" className="grid gap-1 md:sticky md:top-24">
          {items.map((item, index) => <Button key={item.id} type="button" variant="navigation" role="tab"
            id={`${id}-tab-${item.id}`} aria-controls={`${id}-panel-${item.id}`} aria-selected={value === item.id}
            aria-label={item.label} aria-describedby={item.status ? `${id}-status-${item.id}` : undefined}
            tabIndex={value === item.id ? 0 : -1}
            className="h-auto min-w-0 justify-start whitespace-normal px-3 text-left aria-selected:bg-primary/10 aria-selected:text-primary"
            onClick={() => onValueChange(item.id)} onKeyDown={(event) => {
              const next = event.key === "Home" ? 0 : event.key === "End" ? items.length - 1
                : ["ArrowDown", "ArrowRight"].includes(event.key) ? (index + 1) % items.length
                  : ["ArrowUp", "ArrowLeft"].includes(event.key) ? (index + items.length - 1) % items.length : null;
              if (next === null) return;
              event.preventDefault();
              const target = items[next]!;
              onValueChange(target.id); document.getElementById(`${id}-tab-${target.id}`)?.focus();
            }}>
            <span className="min-w-0 flex-1 break-words">{item.label}</span>
            {item.status && <><span data-slot="sidebar-status-dot" aria-hidden="true" className="h-2 w-2 shrink-0 rounded-full bg-success" /><span id={`${id}-status-${item.id}`} className="sr-only">{item.status}</span></>}
          </Button>)}
        </div>
      </aside>}
      <div data-slot="page-sidebar-content" className="grid min-w-0 content-start gap-6 md:col-start-1 md:row-start-1">{children}</div>
    </div>
  </SidebarContext.Provider>;
}

export function PageSidebarPanel({ value, lazy = false, children }: { value: string; lazy?: boolean; children: ReactNode }) {
  const sidebar = useContext(SidebarContext);
  if (!sidebar) throw new Error("PageSidebarPanel requires PageSidebar");
  const selected = sidebar.selected === value;
  return <div role="tabpanel" id={`${sidebar.id}-panel-${value}`} aria-labelledby={`${sidebar.id}-tab-${value}`} hidden={!selected} tabIndex={0} className="grid min-w-0 content-start gap-6">
    {(!lazy || selected || sidebar.visited.has(value)) && children}
  </div>;
}
