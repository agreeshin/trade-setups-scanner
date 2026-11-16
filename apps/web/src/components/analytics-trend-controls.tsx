import { Button } from "./ui/button";
import { Field } from "./ui/field";
import { Select } from "./ui/select";
import { Input } from "./ui/input";
import type { TrendBendDays, TrendMode } from "../lib/analytics-api";
import type { StudyTrendMode } from "../lib/optimization-api";

/** One controls contract for Analytics, manual and continuous study settings. */
export function AnalyticsTrendControls({ mode, bend, onModeChange, onBendChange, disableMeanBend = false, onBlendModeChange, blendLambda = "0.7", onBlendLambdaChange }: {
  mode: StudyTrendMode; bend: TrendBendDays;
  onModeChange: (mode: TrendMode) => void; onBendChange: (bend: TrendBendDays) => void;
  disableMeanBend?: boolean;
  onBlendModeChange?: () => void; blendLambda?: string; onBlendLambdaChange?: (value: string) => void;
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
      {onBlendModeChange && <Button type="button" variant={mode === "blend" ? "default" : "outline"} aria-pressed={mode === "blend"} onClick={onBlendModeChange}>Смесь среднего и прогноза</Button>}
    </div>
    {mode === "blend" && onBlendLambdaChange && <Field label="Доля прогноза λ" hint="0.05–0.95, шаг 0.01. Default 0.7: 30% среднего / 70% прогноза." className="w-64">
      {(control) => <Input {...control} type="number" min={0.05} max={0.95} step={0.01} value={blendLambda} onChange={(event) => onBlendLambdaChange(event.target.value)} />}
    </Field>}
  </div>;
}
