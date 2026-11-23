import { afterEach, expect, it, vi } from "vitest";
import { fetchSetupsProjection, type SetupsProjectionRequest } from "./setups-projection-api";
import { forecastViewFixture } from "../../test/setups-forecast-fixture";

const cutoff = Date.parse("2026-11-13T12:00:00Z") / 1000;
const request: SetupsProjectionRequest = { snapshot: { snapshotId: 50, observationCutoff: cutoff, fingerprint: "a".repeat(64) },
  algorithmId: "price_volume_spike", fromUnix: cutoff - 7 * 86400, toUnix: cutoff, studies: [40], q: "%_", expectedSelected: 5,
  forecast: { method: "trend", version: 1, bendDays: "linear" }, trendMode: "forecast", trendBlendLambda: 0.7, timeZone: "UTC" };
const stub = (body: unknown, status = 200) => vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify(body), { status }))));
afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });

function unavailableNowFixture(trendMode: "forecast" | "blend") {
  const input: SetupsProjectionRequest = { ...request, trendMode, trendBlendLambda: 0.5,
    forecast: { method: "decayed_probability", version: 1, halfLifeDays: 0.005,
      kappa: 1.2446030555722283e-60, priorPolicy: "gross_break_even" } };
  const value = forecastViewFixture(input);
  // forecast_view.rs::compute retains historical evaluations and a null cutoff
  // point, but does not append now when the decision is unavailable. Two TP
  // outcomes with q=3 in the Nov 7 UTC cohort give F=1.5 the next day with
  // kappa=2^-199; their weights have underflowed to zero by this Nov 13 cutoff.
  const historical = { atUnix: Date.parse("2026-11-08T00:00:00Z") / 1000,
    forecastR: 1.5, decisionR: trendMode === "forecast" ? 1.5 : 2.25 };
  const unavailable = { status: "unavailable", reason: "group_forecast_unavailable" };
  return { input, historical, value: { ...value, meanR: 3, forecastR: null, decisionR: null,
    availability: unavailable, forecastAvailability: unavailable,
    model: { ...value.model, method: "decayed_probability",
      points: [historical, { atUnix: cutoff, forecastR: null, decisionR: null }],
      endpoint: { atUnix: value.nowUnix, forecastR: null, decisionR: null } } } };
}

it("requests the entire fixed cohort fingerprint and original filters, without page rows or outcome refresh", async () => {
  stub(forecastViewFixture(request));
  const value = await fetchSetupsProjection(request, new AbortController().signal);
  expect(value).toMatchObject({ meanR: 1.25, forecastR: 3.75, decisionR: 3.75, forecastTarget: "daily_sum_r" });
  const [url, init] = vi.mocked(fetch).mock.calls[0]!;
  if (typeof url !== "string" || typeof init?.body !== "string") throw new Error("Expected a string request URL/body");
  const params = new URL(url, "https://fixture").searchParams;
  expect(params.get("studies")).toBe("40"); expect(params.get("q")).toBe("%_"); expect(params.has("offset")).toBe(false);
  expect(url).toContain("/v1/setups/analytics/forecast-view?"); expect(init.method).toBe("POST");
  expect(JSON.parse(init.body)).toEqual({ snapshot: request.snapshot, forecast: request.forecast,
    trendMode: request.trendMode, trendBlendLambda: request.trendBlendLambda, timeZone: request.timeZone });
  expect(vi.mocked(fetch)).toHaveBeenCalledTimes(1);
});

it("accepts the explicit sum-R recipe only with the same contextual daily-sum target as legacy Setups trend", async () => {
  const explicit = { ...request, forecast: { method: "trend_sum_r", version: 1, bendDays: "linear" } } as const;
  stub(forecastViewFixture(explicit));
  expect(await fetchSetupsProjection(explicit, new AbortController().signal)).toMatchObject({ forecastTarget: "daily_sum_r", forecastR: 3.75 });
  const invalid = forecastViewFixture(explicit); invalid.model.target = "daily_mean_r"; stub(invalid);
  await expect(fetchSetupsProjection(explicit, new AbortController().signal)).rejects.toThrow("target");
});

it.each(["snapshot", "recipe", "lambda", "target", "selected", "endpoint"])("rejects a mismatched %s rather than plotting a stale/incompatible model", async (field) => {
  const value = forecastViewFixture(request);
  if (field === "snapshot") value.snapshot = { ...value.snapshot, fingerprint: "c".repeat(64) };
  if (field === "recipe") value.forecast = { method: "trend", version: 1, bendDays: "7" };
  if (field === "lambda") value.trendBlendLambda = 0.4;
  if (field === "target") value.model.target = "daily_mean_r";
  if (field === "selected") value.selected = 1;
  if (field === "endpoint") value.model.endpoint = { ...value.model.endpoint, decisionR: 999 };
  stub(value); await expect(fetchSetupsProjection(request, new AbortController().signal)).rejects.toThrow();
});

it("preserves unavailable without creating a zero point or a forecast scalar", async () => {
  const value = forecastViewFixture(request);
  stub({ ...value, closed: 0, meanR: null, forecastR: null, decisionR: null,
    availability: { status: "unavailable", reason: "no_closed_outcomes" }, forecastAvailability: { status: "unavailable", reason: "no_closed_outcomes" },
    model: { ...value.model, points: [], endpoint: { ...value.model.endpoint, forecastR: null, decisionR: null } } });
  const result = await fetchSetupsProjection(request, new AbortController().signal);
  expect(result.decisionR).toBeNull(); expect(result.points).toEqual([]); expect(result.availability).toEqual({ status: "unavailable", reason: "no_closed_outcomes" });
});

it.each(["forecast", "blend"] as const)("accepts valid historical %s points when the producer explicitly marks now unavailable", async (mode) => {
  const { input, historical, value } = unavailableNowFixture(mode);
  stub(value);
  const result = await fetchSetupsProjection(input, new AbortController().signal);
  expect(result).toMatchObject({ meanR: 3, forecastR: null, decisionR: null, forecastTarget: "closed_trade_r",
    availability: { status: "unavailable", reason: "group_forecast_unavailable" },
    forecastAvailability: { status: "unavailable", reason: "group_forecast_unavailable" } });
  expect(result.points).toEqual([{ atUnix: historical.atUnix, value: historical.decisionR }]);
  expect(result.points.every((p) => p.atUnix < cutoff)).toBe(true);
  expect(vi.mocked(fetch)).toHaveBeenCalledTimes(1);
});

it.each(["reason", "numeric now", "endpoint", "historical number", "order", "cutoff point", "now point", "forecast availability"])(
  "still rejects malformed unavailable-now wire data: %s", async (field) => {
    const { input, historical, value } = unavailableNowFixture("forecast");
    let invalid: unknown = value;
    if (field === "reason") invalid = { ...value, availability: { status: "unavailable" } };
    if (field === "numeric now") invalid = { ...value, decisionR: 1.5,
      model: { ...value.model, endpoint: { ...value.model.endpoint, decisionR: 1.5 } } };
    if (field === "endpoint") invalid = { ...value, model: { ...value.model,
      endpoint: { ...value.model.endpoint, decisionR: 1.5 } } };
    if (field === "historical number") invalid = { ...value, model: { ...value.model,
      points: [{ ...historical, decisionR: "1.5" }, value.model.points[1]] } };
    if (field === "order") invalid = { ...value, model: { ...value.model, points: [...value.model.points].reverse() } };
    if (field === "cutoff point") invalid = { ...value, model: { ...value.model,
      points: [{ ...historical, atUnix: cutoff }] } };
    if (field === "now point") invalid = { ...value, model: { ...value.model,
      points: [{ ...historical, atUnix: value.nowUnix }] } };
    if (field === "forecast availability") invalid = { ...value, forecastAvailability: { status: "available" } };
    stub(invalid);
    await expect(fetchSetupsProjection(input, new AbortController().signal)).rejects.toThrow();
  },
);

it("reports changed snapshot explicitly without retry, new calculation or outcome mutation", async () => {
  stub({ error: "setups_analytics_snapshot_changed" }, 409);
  await expect(fetchSetupsProjection(request, new AbortController().signal)).rejects.toThrow("исходы снимка изменились");
  expect(vi.mocked(fetch)).toHaveBeenCalledTimes(1);
});

it("reports generic HTTP failure without a presentation prefix or automatic retry", async () => {
  stub({ error: "internal_server_error" }, 500);
  await expect(fetchSetupsProjection(request, new AbortController().signal)).rejects.toThrow(/^HTTP 500$/);
  expect(vi.mocked(fetch)).toHaveBeenCalledTimes(1);
});
