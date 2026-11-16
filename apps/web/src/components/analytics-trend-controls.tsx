import { Button } from "./ui/button";
import { Field } from "./ui/field";
import { Select } from "./ui/select";
import type { TrendBendDays, TrendMode } from "../lib/analytics-api";

/** One controls contract for Analytics, manual and continuous study settings. */
export function AnalyticsTrendControls({ mode, bend, onModeChange, onBendChange, disableMeanBend = false }: {
  mode: TrendMode; bend: TrendBendDays;
  onModeChange: (mode: TrendMode) => void; onBendChange: (bend: TrendBendDays) => void;
  disableMeanBend?: boolean;
}) {
  return <div className="flex flex-wrap items-end gap-3">
    <Field label="Перегиб на N дней" className="w-48">
      {(control) => <Select {...control} disabled={disableMeanBend && mode === "mean"} value={bend}
        onChange={(event) => onBendChange(event.target.value as TrendBendDays)}>
        <option value="7">7 дней</option><option value="14">14 дней</option><option value="30">30 дней</option><option value="linear">∞ — прямая</option>
      </Select>}
    </Field>
    <div role="group" aria-label="Значения матрицы" className="flex flex-wrap gap-1">
      <Button type="button" variant={mode === "mean" ? "default" : "outline"} aria-pressed={mode === "mean"} onClick={() => onModeChange("mean")}>Среднее</Button>
      <Button type="button" variant={mode === "forecast" ? "default" : "outline"} aria-pressed={mode === "forecast"} onClick={() => onModeChange("forecast")}>Прогноз по тренду</Button>
    </div>
  </div>;
}
