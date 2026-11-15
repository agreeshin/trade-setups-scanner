import type { LucideIcon } from "lucide-react";
import { Button, type ButtonProps } from "./button";
import { Tooltip } from "./tooltip";

export function IconButton({ label, icon: Icon, ...props }: Omit<ButtonProps, "children" | "asChild" | "size"> & { label: string; icon: LucideIcon }) {
  return <Tooltip content={label}><Button type="button" variant="outline" {...props} size="icon" aria-label={label}><Icon aria-hidden="true" className="size-4" /></Button></Tooltip>;
}
