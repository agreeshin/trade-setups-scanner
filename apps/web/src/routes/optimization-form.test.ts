import { describe, expect, it } from "vitest";
import type { Algorithm } from "./platform-controls";
import { ANALYTICS_BASELINE, ANALYTICS_SEARCH_FIELDS, buildContinuousConfig, buildOptimizationSpec, DEFAULT_AXIS, initialDomains, parseAxis, type ScopeDraft } from "./optimization-form";

const algorithm: Algorithm = {
  id: "unknown", version: "1", enabled: true, name: "Unknown", description: "",
  params: { addedDecimal: "0.25", addedInteger: 2, opaqueFixed: "retained" },
  fields: [
    { key: "addedDecimal", label: "Дробное", type: "decimal", min: 0.1, max: 10, unit: "%", optimization: { enabledByDefault: true, values: ["0.25", "1.5"] } },
    { key: "addedInteger", label: "Целое", type: "integer", min: 1, max: 10, unit: "мин" },
  ],
};
const scope: ScopeDraft = { from: "2026-10-01T00:00", to: "2026-10-02T00:00", symbols: "OLDUSDT", direction: "short", setupMaturity: "0", horizonMinutes: "10080", minClosedPerCell: "1", minSetupsPerDay: "4", tpLevels: DEFAULT_AXIS, slLevels: DEFAULT_AXIS, maxTrials: "256", maxSeconds: "3600", seed: "42" };
const catalog = [{ id: 1, exchange: "bybit", category: "linear", symbol: "OLDUSDT", status: "Delisted" }];

describe("continuous configuration", () => {
  function continuous(overrides: { lookbackDays?: string; minSetupsPerDay?: string; minMeanExpectancy?: string } = {}) {
    return buildContinuousConfig(algorithm, scope, catalog, initialDomains(algorithm.fields, algorithm.params), initialDomains(ANALYTICS_SEARCH_FIELDS, { ...ANALYTICS_BASELINE }), {
      lookbackDays: "30", minSetupsPerDay: "0, 0.6, 4, 4.0", minMeanExpectancy: "0.15", adaptiveEnabled: false, ...overrides,
    });
  }
  it("reuses metadata and analytics, sends a separate unique threshold list and never sends dates or maturity", () => {
    const result = continuous();
    expect(result).toMatchObject({ lookbackDays: 30, minSetupsPerDay: [0, 0.6, 4], minMeanExpectancy: 0.15, adaptiveEnabled: false,
      study: { algorithmParams: algorithm.params, algorithmSearch: { addedDecimal: ["0.25", "1.5"] }, analyticsParams: ANALYTICS_BASELINE, direction: "short" },
    });
    for (const field of ["fromUnix", "toUnix", "setupMaturity", "minSetupsPerDay"]) expect(result.study).not.toHaveProperty(field);
    expect(result.study.analyticsSearch).not.toHaveProperty("setupMaturity");
  });
  it.each([{ lookbackDays: "0" }, { lookbackDays: "NaN" }, { minSetupsPerDay: "" }, { minSetupsPerDay: "1,,2" }, { minSetupsPerDay: "-1" }, { minSetupsPerDay: "Infinity" }, { minMeanExpectancy: "NaN" }])("rejects invalid continuous controls %j", (overrides) => {
    expect(() => continuous(overrides)).toThrow();
  });
  it.each(["0.5", "3660"])("accepts the API's fractional and maximum lookback %s", (lookbackDays) => {
    expect(continuous({ lookbackDays }).lookbackDays).toBe(Number(lookbackDays));
  });
  it.each(["3661", "0.0001", "0.33333"])("rejects unsupported or fractional-second windows %s", (lookbackDays) => {
    expect(() => continuous({ lookbackDays })).toThrow("Последние N дней");
  });
  it("matches the canonical 64-slot limit without changing algorithm search-domain limits", () => {
    expect(continuous({ minSetupsPerDay: Array.from({ length: 64 }, (_, index) => String(index)).join(",") }).minSetupsPerDay).toHaveLength(64);
    expect(() => continuous({ minSetupsPerDay: Array.from({ length: 65 }, (_, index) => String(index)).join(",") })).toThrow("64");
  });
});
function build(overrides: Partial<ScopeDraft> = {}, mutate?: (domains: ReturnType<typeof initialDomains>) => void) {
  const params = initialDomains(algorithm.fields, algorithm.params);
  mutate?.(params);
  return buildOptimizationSpec(algorithm, { ...scope, ...overrides }, catalog, params, initialDomains(ANALYTICS_SEARCH_FIELDS, { ...ANALYTICS_BASELINE }));
}

it("keeps full baseline, typed dynamic defaults and independent axes", () => {
  expect(build({ tpLevels: "25, 100", slLevels: "0.1, 0.5" })).toMatchObject({
    algorithmParams: algorithm.params, algorithmSearch: { addedDecimal: ["0.25", "1.5"] },
    analyticsParams: ANALYTICS_BASELINE,
    analyticsSearch: { clusterEnabled: [false, true], clusterGapMinutes: [240, 480], timeShiftMinutes: [0, 5], priceChangePct: [null, 0, 1] },
    tpLevels: [25, 100], slLevels: [0.1, 0.5],
    setupMaturity: 0, horizonMinutes: 10080, minSetupsPerDay: 4, maxTrials: 256, maxSeconds: 3600,
  });
});
describe("fixed setup frequency gate", () => {
  it.each(["4", "0.6", "2.25", "0"])("sends %s as a number only in fixed scope", (raw) => {
    const result = build({ minSetupsPerDay: raw });
    expect(result.minSetupsPerDay).toBe(Number(raw));
    expect(result.algorithmSearch).not.toHaveProperty("minSetupsPerDay");
    expect(result.analyticsSearch).not.toHaveProperty("minSetupsPerDay");
    expect(result.algorithmParams).not.toHaveProperty("minSetupsPerDay");
    expect(result.analyticsParams).not.toHaveProperty("minSetupsPerDay");
  });
  it.each(["", " ", "NaN", "Infinity", "-Infinity", "-0.1", "0,6", "0x10", "1e2", "9".repeat(310)])("rejects invalid point-decimal threshold %s", (minSetupsPerDay) => {
    expect(() => build({ minSetupsPerDay })).toThrow("Минимум сетапов в день");
  });
});
it("preserves distinct exact decimal thresholds instead of deduplicating through floating point", () => {
  const result = build({}, (drafts) => { drafts.addedDecimal!.values = "1.00000000000000001, 1, 1.00000000000000001"; });
  expect(result.algorithmSearch.addedDecimal).toEqual(["1.00000000000000001", "1"]);
});
it("edits isolated fixed baseline values without requiring algorithm settings writes", () => {
  const result = build({}, (drafts) => { drafts.addedDecimal!.search = false; drafts.addedDecimal!.baseline = "3.75"; drafts.addedInteger!.baseline = "7"; });
  expect(result.algorithmParams).toEqual({ addedDecimal: "3.75", addedInteger: 7, opaqueFixed: "retained" });
  expect(result.algorithmSearch).toEqual({});
  expect(algorithm.params.addedDecimal).toBe("0.25");
});
describe("bounded finite inputs", () => {
  it.each(["", "NaN", "Infinity", "-Infinity", "11", "null", "1,,2"])("rejects invalid decimal candidates %s", (values) => {
    expect(() => build({}, (drafts) => { drafts.addedDecimal!.values = values; })).toThrow();
  });
  it("rejects fractional integer candidates and excessive domains", () => {
    expect(() => build({}, (drafts) => { drafts.addedInteger!.search = true; drafts.addedInteger!.values = "1.5"; })).toThrow("Целое");
    expect(() => build({}, (drafts) => { drafts.addedDecimal!.values = Array.from({ length: 257 }, () => "1").join(","); })).toThrow("256");
  });
  it.each([
    { maxTrials: "4097" }, { maxTrials: "0" }, { maxTrials: "1.5" }, { maxSeconds: "86401" },
    { maxSeconds: "0" }, { minClosedPerCell: "0" }, { horizonMinutes: "0" }, { horizonMinutes: "43201" }, { seed: "NaN" },
    { setupMaturity: "0.15" }, { from: "2026-02-30T00:00" }, { to: scope.from }, { symbols: "" }, { symbols: "MISSING" },
  ])("rejects invalid fixed scope %j", (overrides) => { expect(() => build(overrides)).toThrow(); });
  it("validates SL identically to TP and limits each axis to 64 unique levels", () => {
    expect(() => build({ slLevels: "0" })).toThrow("SL");
    expect(() => parseAxis(Array.from({ length: 65 }, (_, i) => String(i + 1)).join(","), "SL")).toThrow("64");
    expect(() => parseAxis("4, 4.0", "SL")).toThrow("уникальных");
  });
});
