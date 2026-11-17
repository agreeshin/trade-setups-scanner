import { Button } from "./ui/button";
import { Field } from "./ui/field";
import { Select } from "./ui/select";
import { Input } from "./ui/input";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "./ui/collapsible";
import { initialForecastRecipeDraft, type ForecastRecipeDraft } from "../lib/forecast-recipe";
import type { TrendBendDays, TrendMode } from "../lib/analytics-api";
import type { StudyTrendMode } from "../lib/optimization-api";

/** One controls contract for Analytics, manual and continuous study settings.
 * Recipe support is opt-in until the corresponding Analytics transport is available. */
export function AnalyticsTrendControls({ mode, bend, onModeChange, onBendChange, disableMeanBend = false, onBlendModeChange, blendLambda = "0.7", onBlendLambdaChange, forecast, onForecastChange }: {
  mode: StudyTrendMode; bend: TrendBendDays;
  onModeChange: (mode: TrendMode) => void; onBendChange: (bend: TrendBendDays) => void;
  disableMeanBend?: boolean;
  onBlendModeChange?: () => void; blendLambda?: string; onBlendLambdaChange?: (value: string) => void;
  forecast?: ForecastRecipeDraft | undefined; onForecastChange?: (value: ForecastRecipeDraft) => void;
}) {
  const recipeEnabled = onForecastChange !== undefined;
  const showRecipe = recipeEnabled && mode !== "mean";
  const probability = forecast?.method === "decayed_probability" ? forecast : null;
  const showBend = !recipeEnabled || (!probability && (mode !== "mean" || !disableMeanBend));
  return <div className="grid gap-3">
    <div className="flex flex-wrap items-end gap-3">
      <div role="group" aria-label="Значения матрицы" className="flex flex-wrap gap-1">
        <Button type="button" variant={mode === "mean" ? "default" : "outline"} aria-pressed={mode === "mean"} onClick={() => onModeChange("mean")}>Среднее</Button>
        <Button type="button" variant={mode === "forecast" ? "default" : "outline"} aria-pressed={mode === "forecast"} onClick={() => onModeChange("forecast")}>{recipeEnabled ? "Прогноз" : "Прогноз по тренду"}</Button>
        {onBlendModeChange && <Button type="button" variant={mode === "blend" ? "default" : "outline"} aria-pressed={mode === "blend"} onClick={onBlendModeChange}>Смесь среднего и прогноза</Button>}
      </div>
      {showRecipe && <Field label="Метод прогноза" className="w-64" hint={forecast === undefined ? "Legacy: метод не был сохранён, используется прежний тренд. Новый default не применяется автоматически." : "Фиксированный рецепт исследования, не поисковая координата."}>
        {(control) => <Select {...control} value={forecast?.method ?? "trend"} onChange={(event) => onForecastChange?.(event.target.value === "decayed_probability" ? initialForecastRecipeDraft() : { method: "trend", version: 1, bendDays: bend })}>
          <option value="decayed_probability">Вероятность TP с памятью</option>
          <option value="trend">Прежний тренд (сравнение)</option>
        </Select>}
      </Field>}
      {showRecipe && probability && <Field label="Память модели H, дней" className="w-64" hint="Период полураспада веса UTC-когорт. Конечное число больше 0; фиксируется до исследования.">
        {(control) => <Input {...control} type="number" min={0} step="any" value={probability.halfLifeDays} onChange={(event) => onForecastChange?.({ ...probability, halfLifeDays: event.target.value })} />}
      </Field>}
      {showBend && <Field label="Перегиб на N дней" className="w-48">
        {(control) => <Select {...control} disabled={disableMeanBend && mode === "mean"} value={forecast?.method === "trend" ? forecast.bendDays : bend}
          onChange={(event) => {
            const next = event.target.value as TrendBendDays;
            if (forecast?.method === "trend") onForecastChange?.({ ...forecast, bendDays: next });
            else onBendChange(next);
          }}>
          <option value="7">7 дней</option><option value="14">14 дней</option><option value="30">30 дней</option><option value="linear">∞ — прямая</option>
        </Select>}
      </Field>}
      {mode === "blend" && onBlendLambdaChange && <Field label="Доля прогноза λ" hint="0.05–0.95, шаг 0.01. Default 0.7: 30% среднего / 70% прогноза." className="w-64">
        {(control) => <Input {...control} type="number" min={0.05} max={0.95} step={0.01} value={blendLambda} onChange={(event) => onBlendLambdaChange(event.target.value)} />}
      </Field>}
    </div>
    {showRecipe && probability && <Collapsible className="grid gap-2">
      <CollapsibleTrigger type="button" className="text-sm">Расширенные настройки модели</CollapsibleTrigger>
      <CollapsibleContent className="grid gap-3 sm:grid-cols-2">
        <Field label="Сила prior κ" hint="Конечное число больше 0. Сила регуляризации, не число реально закрытых сделок.">
          {(control) => <Input {...control} type="number" min={0} step="any" value={probability.kappa} onChange={(event) => onForecastChange?.({ ...probability, kappa: event.target.value })} />}
        </Field>
        <Field label="Политика prior" hint="Явно эвристический gross-break-even anchor для своей TP/SL-пары. Не независимая историческая база.">
          {(control) => <Select {...control} value={probability.priorPolicy} onChange={() => onForecastChange?.({ ...probability, priorPolicy: "gross_break_even" })}>
            <option value="gross_break_even">Gross-break-even (эвристический)</option>
          </Select>}
        </Field>
        <p className="text-xs text-muted-foreground sm:col-span-2">Рецепт v{probability.version}. H 14 дней и κ 10 — инженерный baseline, не калиброванный оптимум. H, κ, prior и λ фиксированы до исследования и не подбираются этим оптимизатором. Telegram наследует сохранённый рецепт, отдельных настроек модели нет.</p>
      </CollapsibleContent>
    </Collapsible>}
  </div>;
}
