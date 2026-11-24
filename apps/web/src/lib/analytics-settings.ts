import { apiFetch } from "./platform-api";
import { nextZonedDay, parseZonedDateTimeInput, zonedDayStart } from "./datetime-format";
import type { TrendBendDays, TrendMode } from "./analytics-api";
import { restoreForecastRecipeDraft, type ForecastRecipeDraft } from "./forecast-recipe";

/** One exact signal-close range per direction; month is only a UI preset. */
export interface AnalyticsTimeRange {
  mode: "all" | "month" | "custom";
  month: string;
  fromUnix: number | null;
  toUnix: number | null;
}
export interface AnalyticsPeriod { fromUnix: number; toUnix: number }
const minuteUnix = (raw: unknown): number | null => typeof raw === "number" && Number.isSafeInteger(raw)
  && Number.isFinite(new Date(raw * 1000).getTime()) ? Math.floor(raw / 60) * 60 : null;

function localDay(day: string): number | null {
  const noon = parseZonedDateTimeInput(`${day}T12:00`);
  return Number.isFinite(noon) ? zonedDayStart(noon) : null;
}

function utcDay(day: string): number | null {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(day)) return null;
  const at = Date.parse(`${day}T00:00:00Z`);
  return Number.isFinite(at) && new Date(at).toISOString().slice(0, 10) === day ? at / 1000 : null;
}

/** Dataset timestamps are inclusive. Keep the former full-day default in the UI zone. */
export function allAnalyticsRange(period: AnalyticsPeriod | null): AnalyticsTimeRange {
  return { mode: "all", month: "all", fromUnix: period ? zonedDayStart(period.fromUnix) : null,
    toUnix: period ? nextZonedDay(period.toUnix) : null };
}

export function monthAnalyticsRange(month: string): AnalyticsTimeRange {
  if (!/^\d{4}-(0[1-9]|1[0-2])$/.test(month)) throw new Error("Некорректный месяц.");
  const next = new Date(`${month}-01T12:00:00Z`);
  next.setUTCMonth(next.getUTCMonth() + 1);
  return { mode: "month", month, fromUnix: localDay(`${month}-01`), toUnix: localDay(next.toISOString().slice(0, 10)) };
}

/** Lazy in-memory migration only: loading does not write back or calculate anything. */
export function normalizeAnalyticsTimeRange(raw: unknown, period: AnalyticsPeriod | null = null): AnalyticsTimeRange {
  if (raw === null || typeof raw !== "object") return allAnalyticsRange(period);
  const value = raw as Record<string, unknown>;
  const mode = value.mode === "custom" || value.mode === "month" ? value.mode : "all";
  const month = mode === "month" && typeof value.month === "string" ? value.month : mode;
  // Explicit instants win over civil dates and survive timezone changes/remounts.
  if ("fromUnix" in value || "toUnix" in value) {
    return { mode, month, fromUnix: minuteUnix(value.fromUnix), toUnix: minuteUnix(value.toUnix) };
  }
  if (mode === "custom" && typeof value.fromDay === "string" && typeof value.toDay === "string") {
    // Legacy backend day/month filters were UTC, not interface-local calendars.
    const to = utcDay(value.toDay);
    return { mode, month, fromUnix: utcDay(value.fromDay), toUnix: to === null ? null : to + 86400 };
  }
  if (mode === "month" && /^\d{4}-(0[1-9]|1[0-2])$/.test(month)) {
    const next = new Date(`${month}-01T00:00:00Z`);
    next.setUTCMonth(next.getUTCMonth() + 1);
    return { mode, month, fromUnix: utcDay(`${month}-01`), toUnix: next.getTime() / 1000 };
  }
  return allAnalyticsRange(period);
}

export function requireAnalyticsRange(value: AnalyticsTimeRange): AnalyticsPeriod {
  const { fromUnix, toUnix } = value;
  if (fromUnix === null || toUnix === null || !Number.isSafeInteger(fromUnix) || !Number.isSafeInteger(toUnix) || fromUnix >= toUnix
    || fromUnix % 60 !== 0 || toUnix % 60 !== 0) {
    throw new Error("Укажите корректные даты: конец диапазона должен быть позже начала.");
  }
  return { fromUnix, toUnix };
}

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
  // Keep matrix ranges in their stored representation. Shared forecast saves
  // serialize this document too; only the range editor may migrate legacy UTC.
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
