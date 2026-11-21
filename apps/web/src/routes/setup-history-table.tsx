import { useEffect, useRef, useState, type ReactNode } from "react";
import { Badge } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "../components/ui/dialog";
import { EmptyState } from "../components/ui/empty-state";
import { Pagination } from "../components/ui/pagination";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow, TableSkeletonRows } from "../components/ui/table";
import { fetchSetupDetails, fetchSetupHistory, type SetupDetailsResponse, type SetupHistoryFilter, type SetupHistoryPage } from "../lib/setups-analytics-api";
import { errorText, localTime } from "./platform-controls";
import { useRouteActive } from "./route-active";
import { formatOutcomeRisk, outcomeMeanClass } from "./setup-outcomes-table";
import { SetupSavedDetails } from "./setup-table";

function TelegramText({ message }: { message: NonNullable<SetupDetailsResponse["telegramMessage"]> }) {
  const parts: ReactNode[] = [];
  let position = 0;
  // Telegram offsets are UTF-16, exactly the indexing used by JS slice.
  for (const entity of message.entities) {
    parts.push(message.text.slice(position, entity.offset));
    const text = message.text.slice(entity.offset, entity.offset + entity.length);
    parts.push(entity.type === "bold" ? <strong key={entity.offset}>{text}</strong> : <code key={entity.offset} className="font-mono">{text}</code>);
    position = entity.offset + entity.length;
  }
  parts.push(message.text.slice(position));
  return <pre className="whitespace-pre-wrap break-words font-sans text-sm">{parts}</pre>;
}

function SetupDetailsDialog({ selection, close, restoreFocus }: { selection: { id: number; symbol: string }; close: () => void; restoreFocus: () => void }) {
  const active = useRouteActive();
  const [data, setData] = useState<SetupDetailsResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (!active || data !== null) return;
    const controller = new AbortController();
    void fetchSetupDetails(selection.id, controller.signal).then((value) => {
      if (!controller.signal.aborted) setData(value);
    }).catch((cause) => { if (!controller.signal.aborted) setError(errorText(cause)); });
    return () => controller.abort();
  }, [selection.id, active, data]);
  return <Dialog open={active} onOpenChange={(open) => { if (!open) close(); }}>
    <DialogContent className="max-w-3xl" onCloseAutoFocus={(event) => { event.preventDefault(); if (active) restoreFocus(); }}>
      <DialogHeader><DialogTitle>Подробности {selection.symbol}</DialogTitle><DialogDescription>Сохранённый сетап. Текст Telegram — канонический, время в нём указано в UTC; просмотр ничего не отправляет.</DialogDescription></DialogHeader>
      {error !== null && <p role="alert">{error}</p>}
      {data === null && error === null && <p role="status">Загрузка подробностей…</p>}
      {data !== null && <div className="grid gap-4">
        {data.telegramMessage === null ? <p>Текст Telegram недоступен для сохранённой записи ({data.reason ?? "неподдерживаемый формат"}).</p> : <TelegramText message={data.telegramMessage} />}
        <div className="border-t border-input pt-3"><h3 className="font-medium">Сохранённые подробности</h3>
          <p className="text-sm text-muted-foreground">{data.setup.exchange} · {data.setup.algorithmId} · close сигнала: {data.setup.closePrice} · {localTime(data.setup.closeTimeUnix)}</p>
          <SetupSavedDetails setup={data.setup} showOutcome />
        </div>
      </div>}
    </DialogContent>
  </Dialog>;
}

const labels = { tp: "TP", sl: "SL", unclosed: "не закрылась", no_data: "нет данных", gate_failed: "гейт не пройден" };
const price = (value: number | null | undefined) => value === null || value === undefined ? "—" : String(Number(value.toPrecision(6)));

/** One replaceable page and one selected detail snapshot; no retained page cache. */
export function SetupHistoryTable({ filter, revision = 0 }: { filter: SetupHistoryFilter | null; revision?: number }) {
  const active = useRouteActive();
  const identity = filter === null ? "" : JSON.stringify(filter);
  const [paging, setPaging] = useState<{ identity: string; offset: number; snapshotId?: number }>({ identity, offset: 0 });
  const offset = paging.identity === identity ? paging.offset : 0;
  const snapshotId = paging.identity === identity ? paging.snapshotId : undefined;
  const [state, setState] = useState<{ page: SetupHistoryPage; identity: string; offset: number; readAt: number } | null>(null);
  const [settled, setSettled] = useState<{ key: string; error: string | null } | null>(null);
  const [selection, setSelection] = useState<{ id: number; symbol: string } | null>(null);
  const container = useRef<HTMLDivElement>(null);
  const pinnedSnapshot = useRef<{ identity: string; id: number } | null>(null);
  const requestKey = JSON.stringify([identity, offset, snapshotId, revision]);
  const busy = active && identity !== "" && settled?.key !== requestKey;
  const error = settled?.key === requestKey ? settled.error : null;
  useEffect(() => {
    if (!active || identity === "") return;
    const current = JSON.parse(identity) as SetupHistoryFilter;
    const controller = new AbortController();
    const pinned = snapshotId ?? (pinnedSnapshot.current?.identity === identity ? pinnedSnapshot.current.id : undefined);
    void fetchSetupHistory(current, offset, pinned, controller.signal).then((page) => {
      if (!controller.signal.aborted) {
        pinnedSnapshot.current = { identity, id: page.snapshotId };
        setState({ page, identity, offset, readAt: Date.now() / 1000 });
        setSettled({ key: requestKey, error: null });
      }
    }).catch((cause) => { if (!controller.signal.aborted) setSettled({ key: requestKey, error: errorText(cause) }); });
    return () => controller.abort();
  }, [identity, offset, snapshotId, revision, active, requestKey]);
  const page = state?.page;
  const stale = state !== null && (state.identity !== identity || state.offset !== offset);
  const go = (next: number) => { if (page !== undefined && !stale && !busy) setPaging({ identity, offset: next, snapshotId: page.snapshotId }); };
  const restoreFocus = () => (document.getElementById(`setup-details-${selection?.id}`) ?? container.current)?.focus();
  return <div ref={container} role="region" aria-label="История сохранённых сетапов" tabIndex={-1} className="grid min-w-0 gap-2">
    <h3 className="text-sm font-semibold">Сетапы{page !== undefined && <span className="font-normal text-muted-foreground"> · Найдено: {page.total}</span>}</h3>
    <p className="text-xs text-muted-foreground">Полная сохранённая история в пределах фильтров. WATCH и записи без пригодных рекомендованных условий доступны здесь, но не входят в численное матожидание.</p>
    {busy && <p role="status">Загрузка истории…{page !== undefined ? " Показан предыдущий подтверждённый список." : ""}</p>}
    {error !== null && <p role="alert">История недоступна: {error}{page !== undefined ? " Показан последний подтверждённый список." : ""}</p>}
    {state !== null && <p className="text-xs text-muted-foreground">Последнее успешное чтение: {localTime(state.readAt)}{stale ? " · предыдущие фильтры/страница" : ""}</p>}
    {page?.setups.length === 0 && <EmptyState title="Сетапы не найдены." />}
    {((busy && page === undefined) || (page !== undefined && page.setups.length > 0)) && <div className="overflow-x-auto rounded-md border border-input/60"><Table>
      <TableHeader><TableRow><TableHead>Биржа</TableHead><TableHead>Инструмент</TableHead><TableHead>Направление</TableHead><TableHead>Время сетапа</TableHead>
        <TableHead numeric>Вход</TableHead><TableHead numeric>TP</TableHead><TableHead numeric>SL</TableHead><TableHead>Закрытие</TableHead><TableHead numeric>Цена закрытия</TableHead>
        <TableHead numeric>Результат</TableHead><TableHead>Статус</TableHead><TableHead>Исследование</TableHead><TableHead>Подробности</TableHead></TableRow></TableHeader>
      <TableBody>{page === undefined ? <TableSkeletonRows rows={8} columns={13} /> : page.setups.map((setup) => {
        const outcome = setup.tradeOutcome;
        const direction = setup.recommendation?.direction;
        const recommended = direction === "long" || direction === "short";
        return <TableRow key={setup.id}><TableCell>{setup.exchange}</TableCell><TableCell className="font-medium">{setup.symbol}</TableCell>
          <TableCell><Badge variant={direction === "long" ? "success" : direction === "short" ? "danger" : "neutral"}>{recommended ? direction.toUpperCase() : "WATCH"}</Badge></TableCell>
          <TableCell className="whitespace-nowrap">{localTime(setup.closeTimeUnix)}</TableCell><TableCell numeric>{price(outcome?.entryPrice)}</TableCell><TableCell numeric>{price(outcome?.tpPrice)}</TableCell><TableCell numeric>{price(outcome?.slPrice)}</TableCell>
          <TableCell className="whitespace-nowrap">{outcome?.closeTimeUnix === null || outcome?.closeTimeUnix === undefined ? "—" : localTime(outcome.closeTimeUnix)}</TableCell><TableCell numeric>{price(outcome?.closePrice)}</TableCell>
          <TableCell numeric className={outcomeMeanClass(outcome?.riskReturn ?? null)}>{formatOutcomeRisk(outcome?.riskReturn ?? null)}</TableCell>
          <TableCell>{outcome ? labels[outcome.status] : setup.recommendation ? "Не рассчитан / неприменимо" : "неприменимо"}</TableCell>
          <TableCell>{setup.recommendation?.studyId ? `№${setup.recommendation.studyId}` : "—"}</TableCell>
          <TableCell><Button id={`setup-details-${setup.id}`} variant="outline" size="sm" aria-label={`Подробности ${setup.symbol}`} onClick={() => { if (setup.id !== undefined) setSelection({ id: setup.id, symbol: setup.symbol }); }}>Подробности</Button></TableCell></TableRow>;
      })}</TableBody>
    </Table></div>}
    {page !== undefined && <Pagination label="Страницы сетапов" page={Math.floor(page.offset / page.limit) + 1} pages={Math.ceil(page.total / page.limit)}
      prevDisabled={busy || stale || page.offset === 0} nextDisabled={busy || stale || page.offset + page.limit >= page.total}
      onPrev={() => go(page.offset - page.limit)} onNext={() => go(page.offset + page.limit)} />}
    {selection !== null && <SetupDetailsDialog key={selection.id} selection={selection} close={() => setSelection(null)} restoreFocus={restoreFocus} />}
  </div>;
}
