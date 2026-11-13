import { useRef, useState } from "react";
import { localTime, localTimezoneLabel, type SetupRow } from "./platform-controls";
import { Badge } from "../components/ui/badge";
import { EmptyState } from "../components/ui/empty-state";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "../components/ui/table";
import { formatSetupRecommendation } from "../lib/setup-recommendation";
import { SetupRecommendationDialog, SetupRecommendationPreview, type SelectedSetupRecommendation } from "./setup-recommendation-preview";

function SetupDetails({ setup }: { setup: SetupRow }) {
  const metrics = setup.details;
  const ratio = (value: string | null): string => value !== null && value.trim() !== "" && Number.isFinite(Number(value)) ? `${Number(value).toFixed(4)}×` : "нет базы/не определено";
  return <details className="mt-1.5 min-w-48 rounded-sm border border-input/60 bg-muted/20 px-2.5 py-2 text-xs">
    <summary className="cursor-pointer select-none font-medium text-muted-foreground transition-colors hover:text-foreground">Подробности {setup.symbol}</summary>
    <dl className="mt-2 grid gap-2 text-xs">
      {setup.algorithmVersion !== undefined && <div><dt className="text-muted-foreground">Версия алгоритма</dt><dd className="tabular-nums">{setup.algorithmVersion}</dd></div>}
      {metrics ? <>
        <div><dt className="text-muted-foreground">Изменение цены (%)</dt><dd className="tabular-nums">{metrics.growthPct === null ? "не определено" : `${metrics.growthPct}%`}</dd></div>
        <div><dt className="text-muted-foreground">Оборот к длинной базе</dt><dd className="tabular-nums">{ratio(metrics.volumeLongRatio)}</dd></div>
        <div><dt className="text-muted-foreground">Оборот к короткой базе</dt><dd className="tabular-nums">{ratio(metrics.volumeShortRatio)}</dd></div>
        <div><dt className="text-muted-foreground">Сработавшая ветвь объёма (OR)</dt><dd>{metrics.overLong && metrics.overShort ? "Обе базы" : metrics.overLong ? "Длинная база" : metrics.overShort ? "Короткая база" : "Ни одна"}</dd></div>
        <div><dt className="text-muted-foreground">Пропущено минут</dt><dd className="tabular-nums">{metrics.missingMinutes}</dd></div>
      </> : <div><dt className="text-muted-foreground">Метрики сетапа</dt><dd>Не сохранены для этой записи.</dd></div>}
    </dl>
    {setup.params !== undefined && <div className="mt-3"><h3 className="text-xs font-medium">Параметры алгоритма</h3><dl className="mt-1 grid gap-1">{Object.entries(setup.params).map(([key, value]) => <div key={key} className="break-words"><dt className="text-muted-foreground">{key}</dt><dd>{value}</dd></div>)}</dl></div>}
  </details>;
}

export function SetupTable({ setups }: { setups: SetupRow[] }) {
  const [selection, setSelection] = useState<SelectedSetupRecommendation | null>(null);
  const container = useRef<HTMLDivElement>(null);
  return <div ref={container} role="region" aria-label="Список сетапов" tabIndex={-1} className="min-w-0">
    {selection && <SetupRecommendationDialog selection={selection} onClose={() => setSelection(null)} onRestoreFocus={() => (document.getElementById(selection.triggerId) ?? container.current)?.focus()} />}
    {!setups.length ? <EmptyState className="mt-3" title="Сетапы не найдены." /> : <div className="mt-1 overflow-x-auto rounded-md border border-input/60">
    <Table>
      <caption className="px-3 py-2 text-left text-xs text-muted-foreground">Время: {localTimezoneLabel()}</caption>
      <TableHeader>
        <TableRow className="hover:bg-transparent">
          <TableHead>Инструмент</TableHead>
          <TableHead>Тип</TableHead>
          <TableHead>Начало</TableHead>
          <TableHead>Закрытие</TableHead>
          <TableHead className="text-right">Опорная цена close</TableHead>
          <TableHead>Алгоритм</TableHead>
          <TableHead>График</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>{setups.map((setup, index) => <TableRow key={setup.id ?? `${setup.exchange}-${setup.symbol}-${setup.algorithmId}-${setup.closeTimeUnix}-${index}`}>
        <TableCell className="font-medium">{setup.exchange} · {setup.symbol}</TableCell>
        <TableCell><div className="flex flex-wrap gap-1"><Badge title="Исходное нейтральное событие детектора">WATCH</Badge>{setup.recommendation && formatSetupRecommendation(setup)?.kind === "ready" && <Badge variant={setup.recommendation.direction === "short" ? "danger" : "success"} aria-label={`Рекомендация ${setup.recommendation.direction.toUpperCase()}`}>{setup.recommendation.direction.toUpperCase()}</Badge>}</div></TableCell>
        <TableCell className="whitespace-nowrap tabular-nums">{localTime(setup.openTimeUnix)}</TableCell>
        <TableCell className="whitespace-nowrap tabular-nums">{localTime(setup.closeTimeUnix)}</TableCell>
        <TableCell numeric>{setup.closePrice}</TableCell>
        <TableCell><SetupRecommendationPreview setup={setup} onOpen={setSelection} dialogOpen={selection !== null} /><SetupDetails setup={setup} /></TableCell>
        <TableCell>{setup.exchange === "bybit" && setup.category === "linear" ? <div className="flex flex-wrap gap-x-3 gap-y-1"><a className="text-primary underline-offset-4 hover:underline" href={`https://www.tradingview.com/chart/?symbol=${encodeURIComponent(`BYBIT:${setup.symbol}.P`)}`} target="_blank" rel="noopener noreferrer">TradingView</a><a className="text-primary underline-offset-4 hover:underline" href={`https://www.bybit.com/trade/usdt/${encodeURIComponent(setup.symbol)}`} target="_blank" rel="noopener noreferrer">Bybit</a></div> : <span className="text-muted-foreground">Нет ссылки</span>}</TableCell>
      </TableRow>)}</TableBody>
    </Table>
    </div>}
  </div>;
}
