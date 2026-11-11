import { describe, expect, it } from "vitest";
import type { Algorithm } from "./platform-controls";
import { ANALYTICS_BASELINE, ANALYTICS_SEARCH_FIELDS, buildOptimizationSpec, DEFAULT_AXIS, initialDomains, parseAxis, type ScopeDraft } from "./optimization-form";

const algorithm: Algorithm = {
  id: "unknown", version: "1", enabled: true, name: "Unknown", description: "",
  params: { addedDecimal: "0.25", addedInteger: 2, opaqueFixed: "retained" },
  fields: [
    { key: "addedDecimal", label: "Дробное", type: "decimal", min: 0.1, max: 10, unit: "%", optimization: { enabledByDefault: true, values: ["0.25", "1.5"] } },
    { key: "addedInteger", label: "Целое", type: "integer", min: 1, max: 10, unit: "мин" },
  ],
};
const scope: ScopeDraft = { from: "2026-10-01T00:00", to: "2026-10-02T00:00", symbols: "OLDUSDT", direction: "short", setupMaturity: "0", horizonMinutes: "10080", minClosedPerCell: "1", tpLevels: DEFAULT_AXIS, slLevels: DEFAULT_AXIS, maxTrials: "256", maxSeconds: "3600", seed: "42" };
const catalog = [{ id: 1, exchange: "bybit", category: "linear", symbol: "OLDUSDT", status: "Delisted" }];
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
    setupMaturity: 0, horizonMinutes: 10080, maxTrials: 256, maxSeconds: 3600,
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
