import { apiFetch } from "./platform-api";

export type AnalyticsSectionKey = "short-matrix" | "short-trades" | "long-matrix" | "long-trades";

/** Analytics tab settings document stored under the platform `settings` key "web-analytics". */
export interface AnalyticsUserSettings {
  setId: string | null;
  clusterize: boolean;
  maturity: number;
  clusterGapInput: string;
  autoCalc: boolean;
  sections: Partial<Record<AnalyticsSectionKey, Record<string, unknown>>>;
}

export async function fetchAnalyticsSettings(): Promise<AnalyticsUserSettings | null> {
  const response = await apiFetch("/v1/settings/web-analytics");
  if (!response.ok) throw new Error(`HTTP ${response.status}: ${(await response.text()).slice(0, 120)}`);
  const body = (await response.json()) as { settings: Record<string, unknown> | null };
  const raw = body.settings;
  if (raw === null || typeof raw !== "object" || Object.keys(raw).length === 0) return null;
  const partial = raw as Partial<AnalyticsUserSettings>;
  return {
    setId: typeof partial.setId === "string" && partial.setId !== "" ? partial.setId : null,
    clusterize: partial.clusterize !== false,
    maturity: typeof partial.maturity === "number" && Number.isFinite(partial.maturity) ? partial.maturity : 0.5,
    clusterGapInput: typeof partial.clusterGapInput === "string" && partial.clusterGapInput !== "" ? partial.clusterGapInput : "8",
    autoCalc: partial.autoCalc === true,
    sections: partial.sections !== null && typeof partial.sections === "object" ? partial.sections : {},
  };
}

export async function saveAnalyticsSettings(settings: AnalyticsUserSettings): Promise<void> {
  const response = await apiFetch("/v1/settings/web-analytics", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(settings),
  });
  if (!response.ok) throw new Error(`HTTP ${response.status}: ${(await response.text()).slice(0, 120)}`);
}
