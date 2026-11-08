import type { ReactNode } from "react";
import { Button } from "./button";
import { cn } from "../../lib/utils";

export interface PaginationProps {
  page: number;
  pages: number;
  onPrev: () => void;
  onNext: () => void;
  prevDisabled?: boolean;
  nextDisabled?: boolean;
  summary?: ReactNode;
  label?: string;
  prevLabel?: string;
  nextLabel?: string;
  className?: string;
}

export function Pagination({ page, pages, onPrev, onNext, prevDisabled, nextDisabled, summary, label = "Страницы", prevLabel = "Назад", nextLabel = "Далее", className }: PaginationProps) {
  const total = Math.max(1, pages);
  return (
    <nav aria-label={label} data-slot="pagination" className={cn("mt-4 flex flex-wrap items-center gap-3", className)}>
      <Button variant="outline" size="sm" disabled={prevDisabled ?? page <= 1} onClick={onPrev}>{prevLabel}</Button>
      <span aria-live="polite" className="text-sm text-muted-foreground">{summary ?? `Страница ${page} из ${total}`}</span>
      <Button variant="outline" size="sm" disabled={nextDisabled ?? page >= total} onClick={onNext}>{nextLabel}</Button>
    </nav>
  );
}
