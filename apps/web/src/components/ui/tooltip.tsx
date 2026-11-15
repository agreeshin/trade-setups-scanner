import { useEffect, useId, useRef, useState, type ReactElement, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { Slot } from "@radix-ui/react-slot";

/** A single, short-lived popup per trigger. Keyboard focus and touch work too. */
export function Tooltip({ content, children }: { content: ReactNode; children: ReactElement }) {
  const id = useId();
  const trigger = useRef<HTMLSpanElement>(null);
  const popup = useRef<HTMLDivElement>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const [position, setPosition] = useState<{ left: number; top: number; above: boolean } | null>(null);
  function cancelClose() { clearTimeout(timer.current); }
  function show() {
    cancelClose();
    const rect = trigger.current?.getBoundingClientRect();
    if (rect) setPosition({ left: Math.max(12, Math.min(rect.left, window.innerWidth - 332)), top: rect.bottom + 8, above: window.innerHeight - rect.bottom < 160 });
  }
  function hide() { cancelClose(); setPosition(null); }
  function leave() { cancelClose(); timer.current = setTimeout(() => setPosition(null), 120); }
  useEffect(() => () => clearTimeout(timer.current), []);
  const open = position !== null;
  useEffect(() => {
    if (!open) return;
    const close = () => setPosition(null);
    const key = (event: KeyboardEvent) => { if (event.key === "Escape") { event.stopPropagation(); close(); } };
    const outside = (event: PointerEvent) => {
      if (event.target instanceof Node && !trigger.current?.contains(event.target) && !popup.current?.contains(event.target)) close();
    };
    const visibility = new MutationObserver(() => { if (trigger.current?.closest('[hidden], [aria-hidden="true"]')) close(); });
    visibility.observe(document.body, { subtree: true, attributes: true, attributeFilter: ["hidden", "aria-hidden"] });
    document.addEventListener("keydown", key, true);
    document.addEventListener("pointerdown", outside);
    window.addEventListener("resize", close);
    return () => { visibility.disconnect(); document.removeEventListener("keydown", key, true); document.removeEventListener("pointerdown", outside); window.removeEventListener("resize", close); };
  }, [open]);
  return <span ref={trigger} data-slot="tooltip-trigger" className="inline-flex min-w-0 align-middle" onMouseEnter={show} onMouseLeave={leave} onBlur={(event) => {
    if (!event.currentTarget.contains(event.relatedTarget) && !popup.current?.contains(event.relatedTarget)) hide();
  }}>
    <Slot aria-describedby={open ? id : undefined} onFocus={show} onClick={show}>{children}</Slot>
    {position && createPortal(<div ref={(node) => {
      popup.current = node;
      // Measured geometry only; all visual styling stays in Tailwind/tokens.
      node?.style.setProperty("--tooltip-left", `${position.left}px`);
      node?.style.setProperty("--tooltip-offset", `${position.above ? window.innerHeight - position.top + 16 : position.top}px`);
    }} role="tooltip" id={id} data-slot="tooltip" className={`pointer-events-auto fixed left-[var(--tooltip-left)] z-[100] max-h-[70dvh] w-80 max-w-[calc(100vw-1.5rem)] overflow-y-auto rounded-lg border border-input bg-card p-3 text-xs font-normal leading-relaxed text-card-foreground shadow-card ${position.above ? "bottom-[var(--tooltip-offset)]" : "top-[var(--tooltip-offset)]"}`}
      onMouseEnter={cancelClose} onMouseLeave={leave}>{content}</div>, document.body)}
  </span>;
}
