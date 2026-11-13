import { useContext, useEffect, useId, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Button } from "../components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "../components/ui/dialog";
import { formatSetupRecommendation } from "../lib/setup-recommendation";
import type { SetupRow } from "./platform-controls";
import { RouteActiveContext } from "./route-active";

function usePreviewClock(visible: boolean) {
  const [clock, setClock] = useState(() => Date.now() / 1000);
  useEffect(() => {
    if (!visible) return;
    // Display clock only. It never checks an entry condition or controls server work.
    const timer = setInterval(() => setClock(Date.now() / 1000), 1000);
    return () => clearInterval(timer);
  }, [visible]);
  return { clock, refresh: () => setClock(Date.now() / 1000) };
}
const description = (elapsed: boolean) => elapsed ? "Рекомендуемый момент уже прошёл. Выполнение условия и вход не подтверждены."
  : "Цена — close сигнала; TP/SL — проценты от возможного фактического входа. Условие проверяете самостоятельно; ордер не создаётся.";

export interface SelectedSetupRecommendation { setup: SetupRow; triggerId: string }

/** Table-owned selection outlives a refreshed or evicted row, but is released on close. */
export function SetupRecommendationDialog({ selection, onClose, onRestoreFocus }: { selection: SelectedSetupRecommendation; onClose: () => void; onRestoreFocus: () => void }) {
  const active = useContext(RouteActiveContext);
  const { clock } = usePreviewClock(active);
  const result = formatSetupRecommendation(selection.setup, clock);
  if (result?.kind !== "ready") return null;
  const snapshot = selection.setup.recommendation!;
  return <Dialog open={active} onOpenChange={(next) => { if (!next) onClose(); }}>
    <DialogContent className="max-w-xl" onCloseAutoFocus={(event) => { event.preventDefault(); if (active) onRestoreFocus(); }}>
      <DialogHeader><DialogTitle>Условия сетапа {selection.setup.symbol}</DialogTitle><DialogDescription>{description(result.entryState === "elapsed")}</DialogDescription></DialogHeader>
      <div className="grid min-h-0 gap-3 overflow-y-auto pr-1">
        <pre className="whitespace-pre-wrap break-words font-sans text-sm">{result.text}</pre>
        <p className="text-xs text-muted-foreground">Сохранённое исследование #{snapshot.studyId} · поколение #{snapshot.generationId}. Рекомендация не меняется вслед за новым исследованием или переименованием.</p>
      </div>
    </DialogContent>
  </Dialog>;
}

export function SetupRecommendationPreview({ setup, onOpen, dialogOpen }: { setup: SetupRow; onOpen: (selection: SelectedSetupRecommendation) => void; dialogOpen: boolean }) {
  const routeActive = useContext(RouteActiveContext);
  const [hovered, setHovered] = useState(false);
  const [focused, setFocused] = useState(false);
  const [preview, setPreview] = useState<SetupRow | null>(null);
  const trigger = useRef<HTMLSpanElement>(null);
  const tooltip = useRef<HTMLDivElement>(null);
  const tooltipId = useId();
  const triggerId = `${tooltipId}-trigger`;
  const showTooltip = routeActive && (hovered || focused) && !dialogOpen;
  const { clock, refresh } = usePreviewClock(showTooltip);
  const shown = preview ?? setup;
  const result = formatSetupRecommendation(shown, clock);
  if (!result) return <span>{setup.externalName ?? setup.algorithmId}</span>;
  if (result.kind === "invalid") return <div className="grid gap-1"><span>{setup.externalName ?? setup.algorithmId}</span><p role="alert" className="text-xs text-destructive">{result.message}</p></div>;
  const snapshot = shown.recommendation!;
  const capture = () => { refresh(); setPreview((previous) => previous ?? structuredClone(setup)); };
  function leaveHover() { setHovered(false); if (!focused) setPreview(null); }
  return <span ref={trigger} className="inline-flex min-w-0 flex-col items-start gap-1">
    <Button id={triggerId} type="button" variant="outline" size="sm" aria-haspopup="dialog"
      aria-label={`Условия сетапа ${shown.symbol} · ${snapshot.externalName} · исследование ${snapshot.studyId}`}
      aria-describedby={showTooltip ? tooltipId : undefined}
      onClick={() => onOpen({ setup: structuredClone(shown), triggerId })}
      onPointerEnter={(event) => { if (event.pointerType !== "touch") { capture(); setHovered(true); } }}
      onPointerLeave={(event) => { if (!(event.relatedTarget instanceof Node && tooltip.current?.contains(event.relatedTarget))) leaveHover(); }}
      onFocus={() => { capture(); setFocused(true); }} onBlur={() => { setFocused(false); if (!hovered) setPreview(null); }}
      onKeyDown={(event) => { if (event.key === "Escape") { setHovered(false); setFocused(false); setPreview(null); } }}>
      {snapshot.externalName}
    </Button>
    {showTooltip && createPortal(<div ref={tooltip} role="tooltip" id={tooltipId} className="fixed bottom-4 right-4 z-30 grid max-h-96 w-80 max-w-sm gap-2 overflow-y-auto rounded-lg border border-input bg-card p-3 text-card-foreground shadow-card"
      onPointerEnter={() => setHovered(true)} onPointerLeave={(event) => { if (!(event.relatedTarget instanceof Node && trigger.current?.contains(event.relatedTarget))) leaveHover(); }}>
      <pre className="whitespace-pre-wrap break-words font-sans text-xs">{result.text}</pre>
      <p className="text-xs text-muted-foreground">{description(result.entryState === "elapsed")}</p>
    </div>, document.body)}
  </span>;
}
