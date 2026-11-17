import { apiFetch } from "./platform-api";
import type { ForecastRecipe } from "./forecast-recipe";
import type { TrendMode } from "./analytics-api";

export type ProbabilityForecast =
  | { status: "available"; value: { observationCutoff: number; meanR: number; forecastR: number;
    tpProbability: number; priorProbability: number; tpCount: number; slCount: number;
    weightedTp: number; weightedSl: number; weightSum: number } }
  | { status: "unavailable"; reason: "no_closed_outcomes" | "no_weighted_evidence" };

export interface ForecastCurveRequest {
  forecast: ForecastRecipe;
  trendMode: TrendMode;
  trendBlendLambda: number;
  q: number;
  fromUnix: number;
  toUnix: number;
  observationCutoff: number;
  atUnix?: number;
  daily: { dayStartUnix: number; tpCount: number; slCount: number }[];
}

/** All timestamps here are Unix SECONDS. Legacy TrendPoint.day remains epoch days. */
export interface ForecastCurveResponse {
  forecast: ForecastRecipe;
  trendMode: TrendMode;
  q: number;
  fromUnix: number;
  toUnix: number;
  observationCutoff: number;
  retrospective: true;
  estimate: ProbabilityForecast;
  meanR: number | null;
  forecastR: number | null;
  decisionR: number | null;
  curve: { atUnix: number; forecastR: number | null }[];
  blendCurve?: { atUnix: number; decisionR: number | null }[];
  flatFuture: { fromUnix: number; toUnix: number; forecastR: number | null; decisionR: number | null };
  provenance: { kind: "supplied_closed_outcomes"; availability: "asserted_at_cutoff" };
}

export function sameForecastRecipe(value: unknown, expected: ForecastRecipe): boolean {
  if (value === null || typeof value !== "object") return false;
  const actual = value as Partial<ForecastRecipe>;
  if (actual.version !== expected.version || actual.method !== expected.method) return false;
  return actual.method === "decayed_probability" && expected.method === "decayed_probability"
    ? actual.halfLifeDays === expected.halfLifeDays && actual.kappa === expected.kappa && actual.priorPolicy === expected.priorPolicy
    : actual.method === "trend" && expected.method === "trend" && actual.bendDays === expected.bendDays;
}

function validResponse(value: unknown, request: ForecastCurveRequest): value is ForecastCurveResponse {
  if (value === null || typeof value !== "object") return false;
  const response = value as Partial<ForecastCurveResponse>;
  const finite = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);
  const metric = (v: unknown) => v === null || finite(v);
  const count = (v: unknown) => typeof v === "number" && Number.isSafeInteger(v) && v >= 0;
  const estimate = response.estimate;
  const available = estimate?.status === "available" ? estimate.value : null;
  const validEstimate = estimate?.status === "unavailable" ? ["no_closed_outcomes", "no_weighted_evidence"].includes(estimate.reason)
    : available !== null && typeof available === "object" && available.observationCutoff === request.observationCutoff
      && [available.meanR, available.forecastR, available.tpProbability, available.priorProbability, available.weightedTp, available.weightedSl, available.weightSum].every(finite)
      && count(available.tpCount) && count(available.slCount) && available.tpCount + available.slCount > 0
      && available.weightedTp >= 0 && available.weightedSl >= 0 && available.weightSum > 0
      && available.tpProbability >= 0 && available.tpProbability <= 1 && available.priorProbability > 0 && available.priorProbability < 1;
  const flat = response.flatFuture;
  return sameForecastRecipe(response.forecast, request.forecast) && response.trendMode === request.trendMode && response.q === request.q
    && response.fromUnix === request.fromUnix && response.toUnix === request.toUnix && response.observationCutoff === request.observationCutoff
    && response.retrospective === true && validEstimate && [response.meanR, response.forecastR, response.decisionR].every(metric)
    && response.provenance?.kind === "supplied_closed_outcomes" && response.provenance.availability === "asserted_at_cutoff"
    && Array.isArray(response.curve) && response.curve.length <= 3662
    && response.curve.every((point, index, all) => point !== null && Number.isSafeInteger(point.atUnix) && point.atUnix <= request.observationCutoff && metric(point.forecastR) && (index === 0 || point.atUnix >= all[index - 1]!.atUnix))
    && (response.blendCurve === undefined || (Array.isArray(response.blendCurve) && response.blendCurve.length <= 3662
      && response.blendCurve.every((point, index, all) => point !== null && Number.isSafeInteger(point.atUnix) && point.atUnix <= request.observationCutoff && metric(point.decisionR) && (index === 0 || point.atUnix >= all[index - 1]!.atUnix))))
    && flat !== null && typeof flat === "object" && flat.fromUnix === request.observationCutoff && flat.toUnix === (request.atUnix ?? request.observationCutoff)
    && flat.forecastR === response.forecastR && flat.decisionR === response.decisionR;
}

export async function fetchForecastCurve(request: ForecastCurveRequest, signal: AbortSignal): Promise<ForecastCurveResponse> {
  const response = await apiFetch("/v1/analytics/forecast", {
    method: "POST", signal, headers: { "Content-Type": "application/json" }, body: JSON.stringify(request),
  });
  if (!response.ok) throw new Error(`HTTP ${response.status}: ${(await response.text()).slice(0, 120)}`);
  const value: unknown = await response.json();
  if (!validResponse(value, request)) throw new Error("Сервер вернул несовместимую модель, cutoff или происхождение графика.");
  return value;
}
