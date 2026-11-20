import { nextZonedDay } from "../lib/datetime-format";

/** One palette/geometry for the Setups Analytics study chart and its retrospective counterpart. */
export const CHART_STYLE = {
  width: 900, height: 320, left: 80, right: 872, top: 66, bottom: 258,
  background: "#121314", foreground: "#eceef0", line: "#b9bec6", positive: "#18cf98", negative: "#f08a7f",
  axis: "#a5a8af", grid: "#2e3238", trend: "#f5b041",
};
export function calendarDays(firstDay: number, lastDay: number): number[] {
  const days: number[] = [];
  if (!Number.isFinite(firstDay) || !Number.isFinite(lastDay)) return days;
  for (let day = firstDay; day <= lastDay;) {
    days.push(day);
    const nextDay = nextZonedDay(day);
    if (!Number.isFinite(nextDay) || nextDay <= day) break;
    day = nextDay;
  }
  return days;
}
