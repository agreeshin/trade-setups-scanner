import type { TradeRow } from "./analytics-api";
import { buildDailyExpectancy, type DailyExpectancyPoint } from "./daily-expectancy";

/** Prepare observed labels only; forecast math and all model curves belong to Rust.
 * TradeRow.closeTimeUnix is the exit candle OPEN, so its label is available +60s.
 * Data is calculation-owned, bounded to the backend's 3661 UTC cohort days. */
export function buildObservedForecastDays(rows: readonly (TradeRow & { fromCache: boolean })[], fromUnix: number, toUnix: number, observationCutoff: number): DailyExpectancyPoint[] {
  if (![fromUnix, toUnix, observationCutoff].every(Number.isSafeInteger) || fromUnix >= toUnix) throw new Error("Некорректные границы или cutoff графика.");
  const observed = rows.filter((row) => row.setup.closeTimeUnix >= fromUnix && row.setup.closeTimeUnix < toUnix && row.setup.closeTimeUnix <= observationCutoff)
    .map((row) => (row.status === "tp" || row.status === "sl") && (row.closeTimeUnix === null || row.closeTimeUnix + 60 > observationCutoff)
      ? { ...row, status: "unclosed", riskReturn: null } : row);
  const days = buildDailyExpectancy(observed);
  if (days.length > 3661) throw new Error("График ограничен 3661 UTC-днём.");
  return days;
}
