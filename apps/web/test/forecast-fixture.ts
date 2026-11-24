import type { ForecastCurveRequest, ForecastCurveResponse } from "../src/lib/forecast-api";

/** Transport/geometry fixture only. Rust owns numerical formula/parity tests. */
export function forecastFixture(request: ForecastCurveRequest): ForecastCurveResponse {
  const curve = request.daily.map((day, index) => ({ atUnix: index === request.daily.length - 1 ? request.observationCutoff : Math.min(day.dayStartUnix + 86400, request.observationCutoff),
    forecastR: index === request.daily.length - 1 ? 0.4 : index === 0 ? -0.2 : 0.15 }));
  const response: ForecastCurveResponse = { forecast: request.forecast, trendMode: request.trendMode, q: request.q,
    fromUnix: request.fromUnix, toUnix: request.toUnix, observationCutoff: request.observationCutoff, retrospective: true,
    estimate: { status: "available", value: { observationCutoff: request.observationCutoff, meanR: 0.8, forecastR: 0.4,
      tpProbability: 0.6, priorProbability: 0.5, tpCount: 3, slCount: 2, weightedTp: 2.5, weightedSl: 1.5, weightSum: 4 } },
    meanR: 0.8, forecastR: 0.4, decisionR: request.trendMode === "blend" ? 0.52 : request.trendMode === "mean" ? 0.8 : 0.4,
    curve, ...(request.trendMode === "blend" ? { trendBlendLambda: request.trendBlendLambda, blendCurve: curve.map((point, index) => ({ atUnix: point.atUnix, decisionR: index === curve.length - 1 ? 0.52 : index === 0 ? 0.1 : 0.3 })) } : {}),
    flatFuture: { fromUnix: request.observationCutoff, toUnix: request.atUnix ?? request.observationCutoff,
      forecastR: 0.4, decisionR: request.trendMode === "blend" ? 0.52 : request.trendMode === "mean" ? 0.8 : 0.4 },
    provenance: { kind: "supplied_closed_outcomes", availability: "asserted_at_cutoff" } };
  return withDisplayCurve(request, response);
}

export function withDisplayCurve(request: ForecastCurveRequest, response: ForecastCurveResponse): ForecastCurveResponse {
  response.displayCurve = response.curve.map((point, index) => ({ ...point, signalDayUnix: request.daily[index]!.dayStartUnix,
    decisionR: request.trendMode === "mean" ? response.meanR : request.trendMode === "forecast" ? point.forecastR : response.blendCurve![index]!.decisionR }));
  return response;
}
