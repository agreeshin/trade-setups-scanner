import type { ForecastCurveRequest, ForecastCurveResponse } from "../src/lib/forecast-api";

/** Transport/geometry fixture only. Rust owns numerical formula/parity tests. */
export function forecastFixture(request: ForecastCurveRequest): ForecastCurveResponse {
  const curve = [
    { atUnix: request.daily[0]?.dayStartUnix ?? request.fromUnix, forecastR: -0.2 },
    { atUnix: request.daily.at(-1)?.dayStartUnix ?? request.fromUnix, forecastR: 0.15 },
    { atUnix: request.observationCutoff, forecastR: 0.4 },
  ];
  return { forecast: request.forecast, trendMode: request.trendMode, q: request.q,
    fromUnix: request.fromUnix, toUnix: request.toUnix, observationCutoff: request.observationCutoff, retrospective: true,
    estimate: { status: "available", value: { observationCutoff: request.observationCutoff, meanR: 0.8, forecastR: 0.4,
      tpProbability: 0.6, priorProbability: 0.5, tpCount: 3, slCount: 2, weightedTp: 2.5, weightedSl: 1.5, weightSum: 4 } },
    meanR: 0.8, forecastR: 0.4, decisionR: request.trendMode === "blend" ? 0.52 : request.trendMode === "mean" ? 0.8 : 0.4,
    curve, ...(request.trendMode === "blend" ? { blendCurve: curve.map((point, index) => ({ atUnix: point.atUnix, decisionR: [0.1, 0.3, 0.52][index]! })) } : {}),
    flatFuture: { fromUnix: request.observationCutoff, toUnix: request.atUnix ?? request.observationCutoff,
      forecastR: 0.4, decisionR: request.trendMode === "blend" ? 0.52 : request.trendMode === "mean" ? 0.8 : 0.4 },
    provenance: { kind: "supplied_closed_outcomes", availability: "asserted_at_cutoff" } };
}
