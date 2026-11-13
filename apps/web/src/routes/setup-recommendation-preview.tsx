import { useContext, useEffect, useId, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Button } from "../components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle, DialogTrigger } from "../components/ui/dialog";
import { formatSetupRecommendation } from "../lib/setup-recommendation";
import type { SetupRow } from "./platform-controls";
import { RouteActiveContext } from "./route-active";

export function SetupRecommendationPreview({ setup }: { setup: SetupRow }) {
  const routeActive = useContext(RouteActiveContext);
  const [hovered, setHovered] = useState(false);
  const [focused, setFocused] = useState(false);
  const [open, setOpen] = useState(false);
  const [clock, setClock] = useState(() => Date.now() / 1000);
  const trigger = useRef<HTMLSpanElement>(null);
  const tooltip = useRef<HTMLDivElement>(null);
  const tooltipId = useId();
  const visible = routeActive && (hovered || focused || open);
  useEffect(() => {
    if (!visible) return;
    // Display clock only. It never checks an entry condition or controls server work.
    const timer = setInterval(() => setClock(Date.now() / 1000), 1000);
    return () => clearInterval(timer);
  }, [visible]);
  const result = formatSetupRecommendation(setup, clock);
  if (!result) return <span>{setup.externalName ?? setup.algorithmId}</span>;
  if (result.kind === "invalid") return <div className="grid gap-1"><span>{setup.externalName ?? setup.algorithmId}</span><p role="alert" className="text-xs text-destructive">{result.message}</p></div>;
  const snapshot = setup.recommendation!;
  const showTooltip = routeActive && (hovered || focused) && !open;
  const description = result.entryState === "elapsed" ? "Рекомендуемый момент уже прошёл. Выполнение условия и вход не подтверждены."
    : "Цена — close сигнала; TP/SL — проценты от возможного фактического входа. Условие проверяете самостоятельно; ордер не создаётся.";
  return <span ref={trigger} className="inline-flex min-w-0 flex-col items-start gap-1">
    <Dialog open={routeActive && open} onOpenChange={(next) => { setClock(Date.now() / 1000); setOpen(next); }}>
      <DialogTrigger asChild><Button type="button" variant="outline" size="sm"
        aria-label={`Условия сетапа ${setup.symbol} · ${snapshot.externalName} · исследование ${snapshot.studyId}`}
        aria-describedby={showTooltip ? tooltipId : undefined}
        onPointerEnter={(event) => { if (event.pointerType !== "touch") { setClock(Date.now() / 1000); setHovered(true); } }}
        onPointerLeave={(event) => { if (!(event.relatedTarget instanceof Node && tooltip.current?.contains(event.relatedTarget))) setHovered(false); }}
        onFocus={() => { setClock(Date.now() / 1000); setFocused(true); }} onBlur={() => setFocused(false)}
        onKeyDown={(event) => { if (event.key === "Escape") { setHovered(false); setFocused(false); } }}>
        {snapshot.externalName}
      </Button></DialogTrigger>
      <DialogContent className="max-w-xl">
        <DialogHeader><DialogTitle>Условия сетапа {setup.symbol}</DialogTitle><DialogDescription>{description}</DialogDescription></DialogHeader>
        <div className="grid min-h-0 gap-3 overflow-y-auto pr-1">
          <pre className="whitespace-pre-wrap break-words font-sans text-sm">{result.text}</pre>
          <p className="text-xs text-muted-foreground">Сохранённое исследование #{snapshot.studyId} · поколение #{snapshot.generationId}. Рекомендация не меняется вслед за новым исследованием или переименованием.</p>
        </div>
      </DialogContent>
    </Dialog>
    {showTooltip && createPortal(<div ref={tooltip} role="tooltip" id={tooltipId} className="fixed bottom-4 right-4 z-30 grid max-h-96 w-80 max-w-sm gap-2 overflow-y-auto rounded-lg border border-input bg-card p-3 text-card-foreground shadow-card"
      onPointerEnter={() => setHovered(true)} onPointerLeave={(event) => { if (!(event.relatedTarget instanceof Node && trigger.current?.contains(event.relatedTarget))) setHovered(false); }}>
      <pre className="whitespace-pre-wrap break-words font-sans text-xs">{result.text}</pre>
      <p className="text-xs text-muted-foreground">{description}</p>
    </div>, document.body)}
  </span>;
}
