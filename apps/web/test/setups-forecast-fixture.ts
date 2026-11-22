import type { SetupsProjectionRequest } from "../src/lib/setups-projection-api";

type Input = Pick<SetupsProjectionRequest, "snapshot" | "forecast" | "trendMode" | "trendBlendLambda" | "timeZone" | "expectedSelected">;
/** Authored transport values only. Numerical grouping/evaluation is tested in Rust. */
export const forecastViewFixture = (input: Input) => {
  const nowUnix = input.snapshot.observationCutoff + 37;
  const decisionR = input.trendMode === "mean" ? 1.25 : input.trendMode === "blend" ? 3 : 3.75;
  const endpoint = { atUnix: nowUnix, forecastR: 3.75, decisionR };
  return { snapshot: input.snapshot, snapshotId: input.snapshot.snapshotId, projectionIdentity: `setups-forecast-view-v1:${"b".repeat(64)}`,
    selected: input.expectedSelected, closed: 2, availability: { status: "available" }, forecastAvailability: { status: "available" },
    meanR: 1.25, forecastR: 3.75, decisionR, trendMode: input.trendMode, trendBlendLambda: input.trendBlendLambda,
    forecast: input.forecast, timeZone: input.timeZone, observationCutoff: input.snapshot.observationCutoff, nowUnix, retrospective: true,
    model: { target: input.forecast.method === "trend" ? "daily_sum_r" : "closed_trade_r", method: "linear",
      points: [{ atUnix: input.snapshot.observationCutoff - 86400, forecastR: 3.75, decisionR }, endpoint], endpoint } };
};
