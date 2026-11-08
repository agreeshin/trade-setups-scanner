import type { HTMLAttributes, TdHTMLAttributes, ThHTMLAttributes } from "react";
import { cn } from "../../lib/utils";

export function Table({ className, ...props }: HTMLAttributes<HTMLTableElement>) {
  return <table data-slot="table" className={cn("w-full caption-bottom text-sm", className)} {...props} />;
}

export function TableHeader({ className, ...props }: HTMLAttributes<HTMLTableSectionElement>) {
  return <thead data-slot="table-header" className={cn("border-b border-input text-left text-muted-foreground", className)} {...props} />;
}

export function TableBody({ className, ...props }: HTMLAttributes<HTMLTableSectionElement>) {
  return <tbody data-slot="table-body" className={cn("[&_tr:last-child]:border-0", className)} {...props} />;
}

export function TableRow({ className, ...props }: HTMLAttributes<HTMLTableRowElement>) {
  return <tr data-slot="table-row" className={cn("border-b border-input transition-colors hover:bg-muted/50", className)} {...props} />;
}

export interface TableHeadProps extends ThHTMLAttributes<HTMLTableCellElement> {
  numeric?: boolean;
}

export function TableHead({ className, numeric, ...props }: TableHeadProps) {
  return <th data-slot="table-head" className={cn("px-3 py-2.5 text-left align-middle text-xs font-semibold uppercase tracking-wider text-muted-foreground", numeric && "text-right", className)} {...props} />;
}

export interface TableCellProps extends TdHTMLAttributes<HTMLTableCellElement> {
  numeric?: boolean;
}

export function TableCell({ className, numeric, ...props }: TableCellProps) {
  return <td data-slot="table-cell" className={cn("px-3 py-2.5 align-top", numeric && "text-right tabular-nums", className)} {...props} />;
}
