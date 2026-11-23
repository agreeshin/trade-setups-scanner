import { afterEach, expect, it, vi } from "vitest";
import { fetchForecastCurve, type ForecastCurveRequest } from "./forecast-api";
import { fetchDailyTrend, fetchMatrixCell, fetchMatrixCells, fetchMatrixGrid, type TrendProjectionRequest } from "./analytics-api";
import { forecastFixture } from "../../test/forecast-fixture";

afterEach(() => vi.unstubAllGlobals());
const day = Date.parse("2026-01-01T00:00:00Z") / 1000;
const request: ForecastCurveRequest = { forecast: { method: "decayed_probability", version: 1, halfLifeDays: 14, kappa: 10, priorPolicy: "gross_break_even" },
  trendMode: "blend", trendBlendLambda: 0.7, q: 2, fromUnix: day, toUnix: day + 86400 * 3, observationCutoff: day + 86400 * 4, atUnix: day + 86400 * 5,
  daily: [{ dayStartUnix: day, tpCount: 2, slCount: 1 }, { dayStartUnix: day + 86400 * 2, tpCount: 1, slCount: 1 }] };

it("sends authenticated numeric-seconds/count recipe input to the agreed endpoint with owned cancellation", async () => {
  const response = forecastFixture(request);
  const fetch = vi.fn().mockResolvedValue(new Response(JSON.stringify(response)));
  vi.stubGlobal("fetch", fetch);
  const controller = new AbortController();
  expect(await fetchForecastCurve(request, controller.signal)).toEqual(response);
  const [path, init] = fetch.mock.calls[0] as [string, RequestInit];
  expect(path).toBe("/v1/analytics/forecast");
  expect(JSON.parse(init.body as string)).toEqual(request);
  expect(init.signal).toBe(controller.signal);
  expect(new Headers(init.headers).get("x-platform-request")).toBe("1");
});

it.each(["cutoff", "recipe", "population", "future", "nonnumeric", "oversized", "prior"])("rejects incompatible server %s without fallback", async (change) => {
  const response = forecastFixture(request);
  if (change === "cutoff") response.observationCutoff += 1;
  if (change === "recipe" && response.forecast.method === "decayed_probability") response.forecast = { ...response.forecast, halfLifeDays: 28 };
  if (change === "population") response.provenance = { kind: "unknown", availability: "asserted_at_cutoff" } as unknown as typeof response.provenance;
  if (change === "future") response.flatFuture.forecastR = 0;
  if (change === "nonnumeric") Object.assign(response.curve[0]!, { forecastR: "not-a-number" });
  if (change === "oversized") response.curve = Array.from({ length: 3663 }, () => ({ atUnix: day, forecastR: 0.1 }));
  if (change === "prior" && response.estimate.status === "available") response.estimate.value.priorProbability = 0;
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify(response))));
  await expect(fetchForecastCurve(request, new AbortController().signal)).rejects.toThrow("несовместимую");
});

it("accepts nullable historical prefixes and gaps while retaining an available scalar at C", async () => {
  const response = forecastFixture(request);
  response.curve[0]!.forecastR = null;
  response.blendCurve![0]!.decisionR = null;
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify(response))));
  expect(await fetchForecastCurve(request, new AbortController().signal)).toMatchObject({ forecastR: 0.4, decisionR: 0.52,
    curve: [{ atUnix: day, forecastR: null }, { atUnix: day + 86400 * 2, forecastR: 0.15 }, { atUnix: request.observationCutoff, forecastR: 0.4 }] });
});

it.each(["missing", "lambda", "endpoint", "timestamp"])("rejects an available blend scalar with %s objective-specific graph data, not showing a generic F chart", async (change) => {
  const response = forecastFixture(request);
  Object.assign(response, { trendBlendLambda: request.trendBlendLambda });
  if (change === "missing") delete response.blendCurve;
  if (change === "lambda") Object.assign(response, { trendBlendLambda: 0.6 });
  if (change === "endpoint") response.blendCurve!.at(-1)!.decisionR = 0.4;
  if (change === "timestamp") response.blendCurve!.at(-1)!.atUnix -= 1;
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify(response))));
  await expect(fetchForecastCurve(request, new AbortController().signal)).rejects.toThrow("несовместимую");
});

it("accepts all-null model curves as unavailable evidence, not as a broken response or prior-only zero", async () => {
  const response = forecastFixture(request);
  response.estimate = { status: "unavailable", reason: "no_closed_outcomes" };
  response.meanR = null; response.forecastR = null; response.decisionR = null;
  response.curve = response.curve.map((point) => ({ ...point, forecastR: null }));
  response.blendCurve = response.blendCurve!.map((point) => ({ ...point, decisionR: null }));
  response.flatFuture.forecastR = null; response.flatFuture.decisionR = null;
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify(response))));
  expect((await fetchForecastCurve(request, new AbortController().signal)).estimate).toEqual({ status: "unavailable", reason: "no_closed_outcomes" });
});

it("forwards the identical recipe, mode, lambda and seconds cutoff to all existing matrix surfaces", async () => {
  const cell = { tpPct: 2, slPct: 1, avgMovePct: 0.52, riskExpectancy: 0.52, meanR: 0.8, forecastR: 0.4, decisionR: 0.52,
    forecast: request.forecast, observationCutoff: request.observationCutoff };
  const fetch = vi.fn().mockImplementation(() => Promise.resolve(new Response(JSON.stringify({ ...cell, items: [cell] }))));
  vi.stubGlobal("fetch", fetch);
  const params = { set: "run-1", forecast: request.forecast, trendMode: request.trendMode, trendBlendLambda: 0.7, observationCutoff: request.observationCutoff };
  await fetchMatrixCell(params); await fetchMatrixCells(params); await fetchMatrixGrid(params, [2], [1]);
  for (const [url] of fetch.mock.calls) {
    const query = new URL(String(url), "http://localhost").searchParams;
    expect(JSON.parse(query.get("forecast")!)).toEqual(request.forecast);
    expect(query.get("observationCutoff")).toBe(String(request.observationCutoff));
    expect(query.get("trendMode")).toBe("blend"); expect(query.get("trendBlendLambda")).toBe("0.7");
  }
});

it("does not accept an old mean-only matrix payload as proof of a newly requested forecast recipe", async () => {
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify({ items: [{ tpPct: 2, slPct: 1, riskExpectancy: 0.8 }] }))));
  await expect(fetchMatrixGrid({ set: "run-1", forecast: request.forecast, trendMode: "forecast", observationCutoff: request.observationCutoff }, [2], [1])).rejects.toThrow("не подтвердил");
});

it.each(["mean", "forecast", "blend"] as const)("accepts the exact Rust no-fit projection wire fixture in %s without manufacturing F", async (trendMode) => {
  // Same request/response case as analytics.rs::projection_mean_works_without_fit_and_missing_forecast_never_falls_back.
  const points = [{ day: 3, value: null, weight: 1 }, { day: 5, value: 0, weight: 1 }, { day: 7, value: null, weight: 1 }];
  const projection: TrendProjectionRequest = { forecast: { method: "trend_sum_r", version: 1, bendDays: "linear" }, meanR: 0.4, trendMode };
  const response = { trend: null, projection: { ...projection, forecastR: null, decisionR: trendMode === "mean" ? 0.4 : null,
    curve: trendMode === "mean" ? [{ day: 3, value: 0.4 }, { day: 7, value: 0.4 }] : [] } };
  const fetch = vi.fn().mockResolvedValue(new Response(JSON.stringify(response))); vi.stubGlobal("fetch", fetch);
  expect(await fetchDailyTrend(points, "linear", new AbortController().signal, projection)).toEqual(response);
  const [, init] = fetch.mock.calls[0] as [string, RequestInit];
  expect(JSON.parse(init.body as string)).toEqual({ points, trendBendDays: "linear", projection });
});

it("preserves the omitted-projection old trend wire exactly", async () => {
  const fetch = vi.fn().mockResolvedValue(new Response(JSON.stringify({ trend: null }))); vi.stubGlobal("fetch", fetch);
  expect(await fetchDailyTrend([], "linear", new AbortController().signal)).toBeNull();
  const [, init] = fetch.mock.calls[0] as [string, RequestInit];
  expect(JSON.parse(init.body as string)).toEqual({ points: [], trendBendDays: "linear" });
});

it.each(["method", "bend", "mode", "mean", "lambda", "endpoint"])("rejects mismatched trend projection %s without rendering a stale decision curve", async (field) => {
  const projection: TrendProjectionRequest = { forecast: { method: "trend_sum_r", version: 1, bendDays: "linear" }, meanR: 0.4, trendMode: "mean" };
  const actual = { ...projection, forecast: { ...projection.forecast }, forecastR: null, decisionR: 0.4, curve: [{ day: 3, value: 0.4 }, { day: 7, value: 0.4 }] };
  if (field === "method") actual.forecast.method = "trend";
  if (field === "bend") actual.forecast.bendDays = "7";
  if (field === "mode") actual.trendMode = "forecast";
  if (field === "mean") actual.meanR = 0.5;
  if (field === "lambda") actual.trendBlendLambda = 0.5;
  if (field === "endpoint") actual.curve[1]!.value = 0.5;
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify({ trend: null, projection: actual }))));
  await expect(fetchDailyTrend([{ day: 3, value: null, weight: 1 }, { day: 7, value: null, weight: 1 }], "linear", new AbortController().signal, projection)).rejects.toThrow("не подтвердил");
});
