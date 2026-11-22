import { SetupOutcomeBadge } from "../components/entity-status-badge";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "../components/ui/table";
import type { SetupsAnalyticsSetup } from "../lib/setups-analytics-api";
import { localTime, localTimezoneLabel } from "./platform-controls";

/** Display fields shared by issued setups and retrospective artifacts; no fabricated adaptive-profile identity. */
export type SetupOutcomeDisplayRow = Pick<SetupsAnalyticsSetup, "id" | "symbol" | "setupTimeUnix" | "entryPrice" | "tpPrice" | "slPrice" | "closeTimeUnix" | "closePrice" | "riskReturn" | "status" | "studyId">;

function formatPrice(value: number | null): string {
  return value === null ? "—" : String(Number(value.toPrecision(6)));
}
export function formatOutcomeRisk(value: number | null): string {
  return value === null ? "—" : `${value >= 0 ? "+" : ""}${value.toFixed(2)}R`;
}
export function outcomeMeanClass(value: number | null): string {
  return value === null || value === 0 ? "text-muted-foreground" : value > 0 ? "text-success" : "text-danger";
}

export function SetupOutcomeRow({ setup }: { setup: SetupOutcomeDisplayRow }) {
  return <TableRow>
    <TableCell className="font-medium">{setup.symbol}</TableCell>
    <TableCell className="whitespace-nowrap tabular-nums">{localTime(setup.setupTimeUnix)}</TableCell>
    <TableCell numeric>{formatPrice(setup.entryPrice)}</TableCell>
    <TableCell numeric className="text-success">{formatPrice(setup.tpPrice)}</TableCell>
    <TableCell numeric className="text-danger">{formatPrice(setup.slPrice)}</TableCell>
    <TableCell className="whitespace-nowrap tabular-nums">{setup.closeTimeUnix === null ? "—" : localTime(setup.closeTimeUnix)}</TableCell>
    <TableCell numeric>{formatPrice(setup.closePrice ?? null)}</TableCell>
    <TableCell numeric className={(setup.riskReturn ?? 0) >= 0 ? "text-success" : "text-danger"}>{formatOutcomeRisk(setup.riskReturn)}</TableCell>
    <TableCell><SetupOutcomeBadge status={setup.status} /></TableCell>
    <TableCell numeric>{setup.studyId === null ? "—" : `№${setup.studyId}`}</TableCell>
  </TableRow>;
}

export function SetupOutcomesTable({ setups, label }: { setups: readonly SetupOutcomeDisplayRow[]; label?: string }) {
  return <div className="overflow-x-auto rounded-md border border-input/60">
    <Table aria-label={label}>
      <caption className="px-3 py-2 text-left text-xs text-muted-foreground">Время: {localTimezoneLabel()}</caption>
      <TableHeader><TableRow className="hover:bg-transparent">
        <TableHead>Инструмент</TableHead><TableHead>Время сетапа</TableHead><TableHead numeric>Вход</TableHead>
        <TableHead numeric className="text-success">TP</TableHead><TableHead numeric className="text-danger">SL</TableHead>
        <TableHead>Закрытие</TableHead><TableHead numeric>Цена закрытия</TableHead><TableHead numeric>Результат</TableHead>
        <TableHead>Статус</TableHead><TableHead numeric>Исследование</TableHead>
      </TableRow></TableHeader>
      <TableBody>{setups.map((setup) => <SetupOutcomeRow key={setup.id} setup={setup} />)}</TableBody>
    </Table>
  </div>;
}
