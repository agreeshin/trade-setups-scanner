import type { TradeRow } from "./analytics-api";
import { buildDailyExpectancy, type DailyExpectancyPoint } from "./daily-expectancy";

/** Prepare observed labels only; forecast math and all model curves belong to Rust.
 * TradeRow.closeTimeUnix is the exit candle OPEN, so its label is available +60s.
 * Data is calculation-owned, bounded to the backend's 3661 UTC cohort days. */
export function buildObservedForecastDays(rows: readonly (TradeRow & { fromCache: boolean })[], fromUnix: number, toUnix: number, observationCutoff: number,
  scope: { timeShiftMinutes: number; retainFutureCohorts?: boolean } = { timeShiftMinutes: 0 }): DailyExpectancyPoint[] {
  if (![fromUnix, toUnix, observationCutoff].every(Number.isSafeInteger) || fromUnix >= toUnix) throw new Error("Некорректные границы или cutoff графика.");
  if (!Number.isSafeInteger(scope.timeShiftMinutes) || scope.timeShiftMinutes < 0) throw new Error("Некорректный time-shift графика.");
  const observed = rows.filter((row) => row.setup.closeTimeUnix >= fromUnix && row.setup.closeTimeUnix < toUnix
    && (scope.retainFutureCohorts === true || row.setup.closeTimeUnix <= observationCutoff)).flatMap((row) => {
    // Exact resolve_entry contract: the candle at setup.open + shift is known
    // only when it closes (+60s). A later gate rejection was not observable at C
    // and remains NoData/null setup weight, just like the matrix preparation.
    const entryAvailableAt = row.setup.openTimeUnix + scope.timeShiftMinutes * 60 + 60;
    if (!Number.isSafeInteger(entryAvailableAt)) throw new Error("Не удалось подтвердить время входа для cutoff модели.");
    if (entryAvailableAt > observationCutoff) return [{ ...row, status: "no_data", riskReturn: null }];
    if (row.status === "price_change_filtered") return [];
    return [(row.status === "tp" || row.status === "sl") && (row.closeTimeUnix === null || row.closeTimeUnix + 60 > observationCutoff)
      ? { ...row, status: "unclosed", riskReturn: null } : row];
  });
  const days = buildDailyExpectancy(observed);
  if (days.length > 3661) throw new Error("График ограничен 3661 UTC-днём.");
  return days;
}
