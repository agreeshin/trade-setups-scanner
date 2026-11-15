import * as React from "react";
import * as DialogPrimitive from "@radix-ui/react-dialog";
import { cn } from "../../lib/utils";

type DialogContentProps = React.ComponentPropsWithoutRef<typeof DialogPrimitive.Content>;

export const Dialog = DialogPrimitive.Root;
export const DialogTrigger = DialogPrimitive.Trigger;
export const DialogClose = DialogPrimitive.Close;

export function DialogContent({ className, children, ...props }: DialogContentProps) {
  return <DialogPrimitive.Portal>
    {/* Each complete layer shares one stacking context; a later overlay blurs earlier dialogs too. */}
    <DialogPrimitive.Overlay data-slot="dialog-overlay" className="fixed inset-0 z-50 overflow-y-auto bg-black/40 backdrop-blur-[1.5px] [scrollbar-width:none] [&::-webkit-scrollbar]:hidden">
    <div data-slot="dialog-viewport" className="flex min-h-full items-center justify-center p-4 sm:p-6">
    <DialogPrimitive.Content data-slot="dialog-content" className={cn("relative flex w-full min-w-0 max-w-lg shrink-0 flex-col gap-4 rounded-xl border border-input/60 bg-card p-5 text-sm shadow-card sm:p-6", className)} {...props}>
      {children}
      <DialogPrimitive.Close aria-label="Закрыть" className="absolute right-3 top-3 inline-flex size-7 shrink-0 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring">✕</DialogPrimitive.Close>
    </DialogPrimitive.Content>
    </div>
    </DialogPrimitive.Overlay>
  </DialogPrimitive.Portal>;
}

export function DialogHeader({ className, ...props }: React.HTMLAttributes<HTMLDivElement>) {
  return <div data-slot="dialog-header" className={cn("grid gap-1.5 pr-8", className)} {...props} />;
}

export function DialogTitle({ className, ...props }: React.ComponentPropsWithoutRef<typeof DialogPrimitive.Title>) {
  return <DialogPrimitive.Title className={cn("text-base font-semibold leading-tight tracking-tight", className)} {...props} />;
}

export function DialogDescription({ className, ...props }: React.ComponentPropsWithoutRef<typeof DialogPrimitive.Description>) {
  return <DialogPrimitive.Description className={cn("text-sm text-muted-foreground", className)} {...props} />;
}
