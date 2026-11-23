import { apiFetch } from "./platform-api";
import type { TrendBendDays, TrendMode } from "./analytics-api";
import { restoreForecastRecipeDraft, type ForecastRecipeDraft } from "./forecast-recipe";

export type AnalyticsSectionKey = "short-matrix" | "short-trades" | "long-matrix" | "long-trades";

/** Analytics tab settings document stored under the platform `settings` key "web-analytics". */
export interface AnalyticsUserSettings {
  setId: string | null;
  clusterize: boolean;
  maturity: number;
  clusterGapInput: string;
  autoCalc: boolean;
  trendBendDays: TrendBendDays;
  trendMode: TrendMode;
  trendBlendLambda?: string;
  forecast?: ForecastRecipeDraft | undefined;
  sections: Partial<Record<AnalyticsSectionKey, Record<string, unknown>>>;
}

export async function fetchAnalyticsSettings(signal?: AbortSignal): Promise<AnalyticsUserSettings | null> {
  const response = await apiFetch("/v1/settings/web-analytics", signal ? { signal } : {});
  if (!response.ok) throw new Error(`HTTP ${response.status}: ${(await response.text()).slice(0, 120)}`);
  const body = (await response.json()) as { settings: Record<string, unknown> | null };
  const raw = body.settings;
  if (raw === null || typeof raw !== "object" || Object.keys(raw).length === 0) return null;
  const partial = raw as Partial<AnalyticsUserSettings>;
  const forecast = restoreForecastRecipeDraft(partial.forecast);
  return {
    ...raw,
    setId: typeof partial.setId === "string" && partial.setId !== "" ? partial.setId : null,
    clusterize: partial.clusterize !== false,
    maturity: typeof partial.maturity === "number" && Number.isFinite(partial.maturity) ? partial.maturity : 0.5,
    clusterGapInput: typeof partial.clusterGapInput === "string" && partial.clusterGapInput !== "" ? partial.clusterGapInput : "8",
    autoCalc: partial.autoCalc === true,
    trendBendDays: forecast && forecast.method !== "decayed_probability" ? forecast.bendDays
      : partial.trendBendDays === "7" || partial.trendBendDays === "14" || partial.trendBendDays === "30" ? partial.trendBendDays : "linear",
    trendMode: partial.trendMode === "forecast" || partial.trendMode === "blend" ? partial.trendMode : "mean",
    trendBlendLambda: typeof partial.trendBlendLambda === "string" || typeof partial.trendBlendLambda === "number" ? String(partial.trendBlendLambda) : "0.7",
    forecast,
    sections: normalizeAnalyticsSections(partial.sections),
  };
}

/** Retired list filters never become a fallback or get dual-written on autosave. */
function normalizeAnalyticsSections(value: AnalyticsUserSettings["sections"] | undefined): AnalyticsUserSettings["sections"] {
  const sections = value !== null && typeof value === "object" ? { ...value } : {};
  for (const key of ["short-trades", "long-trades"] as const) {
    if (sections[key] === undefined) continue;
    const saved = { ...sections[key] };
    for (const field of ["timeRange", "timeShiftInput", "considerPriceChange", "priceChangeInput"]) delete saved[field];
    sections[key] = saved;
  }
  return sections;
}

export async function saveAnalyticsSettings(settings: AnalyticsUserSettings): Promise<void> {
  const response = await apiFetch("/v1/settings/web-analytics", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(settings),
  });
  if (!response.ok) throw new Error(`HTTP ${response.status}: ${(await response.text()).slice(0, 120)}`);
}
