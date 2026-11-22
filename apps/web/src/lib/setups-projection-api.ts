import { apiFetch } from "./platform-api";
import type { TrendMode } from "./analytics-api";
import { sameForecastRecipe } from "./forecast-api";
import type { ForecastRecipe } from "./forecast-recipe";
import { isSetupsAnalyticsSnapshot, type SetupsAnalyticsSnapshot } from "./setups-analytics-api";

export interface SetupsProjectionRequest {
  /** Full calculated cohort identity, not a history page/cursor. The server re-reads
   * its saved outcomes and fails closed if any fingerprint input has changed. */
  snapshot: SetupsAnalyticsSnapshot;
  algorithmId: string;
  fromUnix: number;
  toUnix: number;
  studies: number[] | null;
  q: string;
  expectedSelected: number;
  timeZone: string;
  trendMode: TrendMode;
  trendBlendLambda: number;
  forecast: ForecastRecipe;
}
type Availability = { status: "available" } | { status: "unavailable"; reason: string };
export interface SetupsProjection {
  snapshot: SetupsAnalyticsSnapshot;
  projectionIdentity: string;
  selected: number;
  closed: number;
  nowUnix: number;
  observationCutoff: number;
  timeZone: string;
  trendMode: TrendMode;
  forecastTarget: "daily_sum_r" | "closed_trade_r";
  /** Factual closed-trade mean is distinct from the daily-sum-R trend target. */
  meanR: number | null;
  forecastR: number | null;
  decisionR: number | null;
  /** Availability at now, independent of earlier finite historical points. */
  availability: Availability;
  forecastAvailability: Availability;
  /** Already evaluated server decisionR points; no fitting/grouping on the client. */
  points: { atUnix: number; value: number }[];
}
const record = (v: unknown): v is Record<string, unknown> => v !== null && typeof v === "object" && !Array.isArray(v);
const finite = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);
const count = (v: unknown): v is number => typeof v === "number" && Number.isSafeInteger(v) && v >= 0;
const metric = (v: unknown): v is number | null => v === null || finite(v);
const availability = (v: unknown): v is Availability => record(v)
  && (v.status === "available" || (v.status === "unavailable" && typeof v.reason === "string" && v.reason.length > 0));
interface ModelPoint { atUnix: number; forecastR: number | null; decisionR: number | null }
const point = (v: unknown): v is ModelPoint => record(v) && count(v.atUnix) && metric(v.forecastR) && metric(v.decisionR);

export async function fetchSetupsProjection(request: SetupsProjectionRequest, signal: AbortSignal): Promise<SetupsProjection> {
  const expectedTarget = request.forecast.method === "trend" ? "daily_sum_r" : "closed_trade_r";
  const query = new URLSearchParams({ algorithmId: request.algorithmId, fromUnix: String(request.fromUnix), toUnix: String(request.toUnix) });
  if (request.studies !== null) query.set("studies", request.studies.join(","));
  if (request.q !== "") query.set("q", request.q);
  const response = await apiFetch(`/v1/setups/analytics/forecast-view?${query}`, { method: "POST", signal,
    headers: { "Content-Type": "application/json" }, body: JSON.stringify({ snapshot: request.snapshot,
      forecast: request.forecast, trendMode: request.trendMode, trendBlendLambda: request.trendBlendLambda, timeZone: request.timeZone }) });
  if (!response.ok) throw new Error(response.status === 409
    ? "Сохранённые исходы снимка изменились. Модель недоступна до нового ручного расчёта; исходы не обновлялись."
    : `Модель недоступна: HTTP ${response.status}`);
  const v: unknown = await response.json();
  if (!record(v) || !isSetupsAnalyticsSnapshot(v.snapshot)
    || v.snapshot.snapshotId !== request.snapshot.snapshotId || v.snapshot.observationCutoff !== request.snapshot.observationCutoff
    || v.snapshot.fingerprint !== request.snapshot.fingerprint || v.snapshotId !== request.snapshot.snapshotId
    || v.observationCutoff !== request.snapshot.observationCutoff || !count(v.nowUnix) || v.nowUnix < v.observationCutoff
    || v.timeZone !== request.timeZone || v.trendMode !== request.trendMode || v.trendBlendLambda !== request.trendBlendLambda
    || !sameForecastRecipe(v.forecast, request.forecast) || v.retrospective !== true
    || typeof v.projectionIdentity !== "string" || !/^setups-forecast-view-v1:[a-f0-9]{64}$/.test(v.projectionIdentity)
    || v.selected !== request.expectedSelected || !count(v.closed) || v.closed > v.selected
    || ![v.meanR, v.forecastR, v.decisionR].every(metric) || !availability(v.availability) || !availability(v.forecastAvailability)
    || !record(v.model) || v.model.target !== expectedTarget
    || typeof v.model.method !== "string" || !point(v.model.endpoint) || v.model.endpoint.atUnix !== v.nowUnix
    || v.model.endpoint.forecastR !== v.forecastR || v.model.endpoint.decisionR !== v.decisionR
    || !Array.isArray(v.model.points) || v.model.points.length > 10000
    || !v.model.points.every((p: unknown, index: number, all: unknown[]) => {
      const previous = all[index - 1];
      return point(p) && p.atUnix <= (v.nowUnix as number) && (index === 0 || (point(previous) && previous.atUnix <= p.atUnix));
    })) {
    throw new Error("Несовместимая модель: снимок, календарь, рецепт, target или численные поля.");
  }
  const points = (v.model.points as ModelPoint[]).flatMap((p) => p.decisionR === null ? [] : [{ atUnix: p.atUnix, value: p.decisionR }]);
  // Probability evidence can be available historically but unavailable at C/now.
  // Keep those points without accepting a numeric cutoff/now or inventing one.
  if (v.availability.status === "available" ? !finite(v.decisionR) || points.at(-1)?.atUnix !== v.nowUnix || points.at(-1)?.value !== v.decisionR
    : v.decisionR !== null || points.some((p) => p.atUnix >= request.snapshot.observationCutoff)) throw new Error("Модель не содержит согласованную оценку на now или причину недоступности.");
  if ((v.forecastAvailability.status === "available") !== finite(v.forecastR)) throw new Error("Несогласованная доступность чистого прогноза F.");
  return { snapshot: v.snapshot, projectionIdentity: v.projectionIdentity, selected: v.selected, closed: v.closed,
    nowUnix: v.nowUnix, observationCutoff: v.observationCutoff, timeZone: v.timeZone, trendMode: request.trendMode,
    forecastTarget: expectedTarget, meanR: v.meanR as number | null, forecastR: v.forecastR as number | null,
    decisionR: v.decisionR as number | null, availability: v.availability, forecastAvailability: v.forecastAvailability, points };
}
