import { expect, it } from "vitest";
import { buildForecastRecipe, forecastRecipeLabel, initialForecastRecipeDraft, restoreForecastRecipeDraft } from "./forecast-recipe";
import { applyOptimizationDraftScope, createOptimizationDraft, restoreOptimizationDraft } from "../routes/optimization-draft";
import { initialScope } from "../routes/optimization-form";

it("uses an explicit probability engineering baseline without changing the default mean mode", () => {
  const scope = initialScope();
  expect(scope.trendMode).toBe("mean");
  expect(buildForecastRecipe(scope.forecast!)).toEqual({ method: "decayed_probability", version: 1, halfLifeDays: 14, kappa: 10, priorPolicy: "gross_break_even" });
  const first = initialForecastRecipeDraft();
  if (first.method === "decayed_probability") first.halfLifeDays = "99";
  expect(initialForecastRecipeDraft()).toHaveProperty("halfLifeDays", "14");
});

it.each(["7", "14", "30", "linear"] as const)("round-trips the actual string trend bend %s", (bendDays) => {
  const recipe = { method: "trend", version: 1, bendDays } as const;
  expect(buildForecastRecipe(restoreForecastRecipeDraft(recipe)!)).toEqual(recipe);
});

it.each(["halfLifeDays", "kappa"] as const)("accepts positive finite %s without fabricated numerical bounds", (key) => {
  const draft = initialForecastRecipeDraft();
  if (draft.method !== "decayed_probability") throw new Error("Expected probability baseline");
  for (const value of ["0.00001", "0.5", "36500", "1e100"]) expect(buildForecastRecipe({ ...draft, [key]: value })).toHaveProperty(key, Number(value));
});

it.each(["", " ", "0", "-1", "NaN", "Infinity", "-Infinity", "1e999"])("rejects invalid active H or kappa %j", (value) => {
  const draft = initialForecastRecipeDraft();
  if (draft.method !== "decayed_probability") throw new Error("Expected probability baseline");
  expect(() => buildForecastRecipe({ ...draft, halfLifeDays: value })).toThrow("Память модели H");
  expect(() => buildForecastRecipe({ ...draft, kappa: value })).toThrow("Сила prior κ");
});

it("preserves absent and null legacy recipes rather than filling a probability default", () => {
  expect(restoreForecastRecipeDraft(undefined)).toBeUndefined();
  expect(restoreForecastRecipeDraft(null)).toBeUndefined();
  expect(forecastRecipeLabel(undefined, "14")).toBe("Прогноз по тренду · перегиб 14 дней");
});

it.each([
  { method: "decayed_probability", version: 2, halfLifeDays: 14, kappa: 10, priorPolicy: "gross_break_even" },
  { method: "decayed_probability", version: 1, halfLifeDays: 14, kappa: 10, priorPolicy: "historical" },
  { method: "decayed_probability", version: 1, halfLifeDays: 14, kappa: 10, priorPolicy: "gross_break_even", bendDays: "7" },
  { method: "trend", version: 1, bendDays: 7 },
  { method: "trend", version: 1, bendDays: "7", kappa: 10 },
  { method: "unknown", version: 1 },
])("rejects an unsupported or mixed saved recipe without silently converting it: %j", (value) => {
  expect(() => restoreForecastRecipeDraft(value)).toThrow("не поддерживается");
});

it("keeps saved draft text, including unfinished inputs, separate from numeric wire settings", () => {
  const draft = initialForecastRecipeDraft();
  if (draft.method !== "decayed_probability") throw new Error("Expected probability baseline");
  const unfinished = { ...draft, halfLifeDays: "", kappa: "0.25" };
  expect(restoreForecastRecipeDraft(unfinished)).toEqual(unfinished);
  expect(() => buildForecastRecipe(unfinished)).toThrow("Память модели H");
  expect(restoreForecastRecipeDraft(buildForecastRecipe({ ...draft, halfLifeDays: "7.5", kappa: "0.25" }))).toEqual({ ...draft, halfLifeDays: "7.5", kappa: "0.25" });
});

it.each(["manual", "continuous"] as const)("restores an old %s draft as legacy despite the new probability fallback", (mode) => {
  const scope = initialScope();
  const draft = createOptimizationDraft(scope, {}, {}, null, { manual: { from: "", to: "", setupMaturity: "0", minSetupsPerDay: "4" } });
  delete draft.shared.scope.forecast;
  draft.shared.scope.trendMode = "forecast";
  draft.shared.scope.trendBendDays = "30";
  const restored = restoreOptimizationDraft(JSON.parse(JSON.stringify(draft)), [], [], scope, {}, {});
  expect(restored).not.toBeNull();
  const result = applyOptimizationDraftScope(scope, restored!, mode);
  expect(result.forecast).toBeUndefined();
  expect(result.trendMode).toBe("forecast");
  expect(result.trendBendDays).toBe("30");
  // Even applying a raw old draft must overwrite the new initial recipe.
  expect(applyOptimizationDraftScope(scope, draft, mode).forecast).toBeUndefined();
});

it("round-trips one shared custom probability draft across manual and continuous forms", () => {
  const scope = { ...initialScope(), trendMode: "blend" as const,
    forecast: { method: "decayed_probability", version: 1, halfLifeDays: "28", kappa: "5", priorPolicy: "gross_break_even" } as const };
  const draft = createOptimizationDraft(scope, {}, {}, null, { manual: { from: "", to: "", setupMaturity: "0", minSetupsPerDay: "4" } });
  const restored = restoreOptimizationDraft(JSON.parse(JSON.stringify(draft)), [], [], initialScope(), {}, {})!;
  for (const mode of ["manual", "continuous"] as const) expect(applyOptimizationDraftScope(initialScope(), restored, mode).forecast).toEqual(scope.forecast);
  expect(draft.shared.scope.forecast).not.toBe(scope.forecast);
  expect(forecastRecipeLabel(buildForecastRecipe(scope.forecast), "7")).toContain("H 28 дней · κ 5");
});
