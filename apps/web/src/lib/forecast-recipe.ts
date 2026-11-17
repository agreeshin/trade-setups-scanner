import type { TrendBendDays } from "./analytics-api";

/** Mirrors domain::forecast::ForecastRecipe. Absence is legacy trend, never a new default. */
export type ForecastRecipe =
  | { method: "decayed_probability"; version: 1; halfLifeDays: number; kappa: number; priorPolicy: "gross_break_even" }
  | { method: "trend"; version: 1; bendDays: TrendBendDays };

/** Form-owned text preserves unfinished edits; convert to the wire recipe only on submit. */
export type ForecastRecipeDraft =
  | { method: "decayed_probability"; version: 1; halfLifeDays: string; kappa: string; priorPolicy: "gross_break_even" }
  | { method: "trend"; version: 1; bendDays: TrendBendDays };

/** Explicit engineering baseline, not a calibrated optimum. Only for genuinely new forms. */
export function initialForecastRecipeDraft(): ForecastRecipeDraft {
  return { method: "decayed_probability", version: 1, halfLifeDays: "14", kappa: "10", priorPolicy: "gross_break_even" };
}

export function isTrendBendDays(value: unknown): value is TrendBendDays {
  return value === "7" || value === "14" || value === "30" || value === "linear";
}

/** Accepts saved draft text or a typed server recipe. Never upgrades legacy or unknown versions. */
export function restoreForecastRecipeDraft(value: unknown): ForecastRecipeDraft | undefined {
  if (value === undefined || value === null) return undefined;
  if (typeof value !== "object" || Array.isArray(value)) throw new Error("Некорректный сохранённый рецепт прогноза.");
  const recipe = value as Record<string, unknown>;
  const only = (keys: string[]) => Object.keys(recipe).every((key) => keys.includes(key));
  if (recipe.version === 1 && recipe.method === "trend" && isTrendBendDays(recipe.bendDays) && only(["method", "version", "bendDays"])) {
    return { method: "trend", version: 1, bendDays: recipe.bendDays };
  }
  if (recipe.version === 1 && recipe.method === "decayed_probability" && recipe.priorPolicy === "gross_break_even"
    && only(["method", "version", "halfLifeDays", "kappa", "priorPolicy"])
    && [recipe.halfLifeDays, recipe.kappa].every((item) => typeof item === "string" || typeof item === "number")) {
    return { method: "decayed_probability", version: 1, halfLifeDays: String(recipe.halfLifeDays), kappa: String(recipe.kappa), priorPolicy: "gross_break_even" };
  }
  throw new Error("Сохранённый метод, версия или prior прогноза не поддерживается. Рецепт не заменён новыми настройками.");
}

function positiveFiniteInput(raw: string, label: string): number {
  const value = Number(raw);
  if (!raw.trim() || !Number.isFinite(value) || value <= 0) throw new Error(`${label}: требуется конечное число больше 0.`);
  return value;
}

export function buildForecastRecipe(draft: ForecastRecipeDraft): ForecastRecipe {
  if (draft.version !== 1) throw new Error("Неподдерживаемая версия рецепта прогноза.");
  if (draft.method === "trend" && isTrendBendDays(draft.bendDays)) return { method: "trend", version: 1, bendDays: draft.bendDays };
  if (draft.method === "decayed_probability" && draft.priorPolicy === "gross_break_even") {
    return { method: "decayed_probability", version: 1,
      halfLifeDays: positiveFiniteInput(draft.halfLifeDays, "Память модели H"),
      kappa: positiveFiniteInput(draft.kappa, "Сила prior κ"), priorPolicy: "gross_break_even" };
  }
  throw new Error("Проверьте метод и параметры рецепта прогноза.");
}

export function forecastRecipeLabel(recipe: ForecastRecipe | undefined, legacyBend: TrendBendDays): string {
  if (recipe?.method === "decayed_probability") return `Вероятность TP с памятью · v${recipe.version} · H ${recipe.halfLifeDays} дней · κ ${recipe.kappa} · prior: эвристический gross-break-even`;
  const bend = recipe?.method === "trend" ? recipe.bendDays : legacyBend;
  return `Прогноз по тренду · ${bend === "linear" ? "∞ — прямая" : `перегиб ${bend} дней`}${recipe ? ` · v${recipe.version}` : ""}`;
}
