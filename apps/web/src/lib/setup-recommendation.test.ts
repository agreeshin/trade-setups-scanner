import { afterEach, beforeEach, expect, it } from "vitest";
import { applyInterfaceSettings } from "./datetime-format";
import { formatSetupRecommendation, type AdaptiveRecommendation } from "./setup-recommendation";

const at = Date.parse("2026-11-12T19:43:00Z") / 1000;
const recommendation: AdaptiveRecommendation = { direction: "short", tpPct: 14, slPct: 4, rawR: 0.32, meanR: 0.21, robustScore: 0.01,
  timeShiftMinutes: 3, priceChangePct: 1, externalName: "Пользовательское имя", studyId: 42, generationId: 7, slotId: 5, ownerId: "admin", clusterEnabled: true, clusterGapMinutes: 480,
  params: { priceGrowthPct: "12", windowMinutes: 5, baselineLongDays: 30, baselineShortDays: 7, volumeMultiplier: "3", cooldownMinutes: 5, maxGapPercent: "1", maxConsecutiveGapMinutes: 5, requiredRecentMinutes: 10 },
  provenance: { algorithmId: "price_volume_spike", algorithmVersion: "1.1.1", optimizerVersion: "continuous-v1", seed: 42, snapshotHash: "frozen", capturedAt: "2026-11-12T19:43:00Z", fromUnix: at - 86400, toUnix: at, observationCutoff: at } };
const setup = { exchange: "bybit", symbol: "COINUSDT", closeTimeUnix: at, closePrice: "100", recommendation };
beforeEach(() => { applyInterfaceSettings({ timezone: "UTC" }); });
afterEach(() => { applyInterfaceSettings({ timezone: "auto" }); });
function text(patch: Partial<AdaptiveRecommendation> = {}, now = at) {
  const result = formatSetupRecommendation({ ...setup, recommendation: { ...recommendation, ...patch } }, now);
  if (result?.kind !== "ready") throw new Error("Expected ready recommendation");
  return result;
}
it("formats the owner template with known signal close, conditional future threshold and RAW selected-cell R", () => {
  const result = text();
  expect(result.text).toContain("🔴 COINUSDT · SHORT · Bybit\nЦена: 100,00 USDT");
  expect(result.text).toContain("19:46:00 UTC (через 3 мин после сигнала)");
  expect(result.text).toContain("🔖 TP = 14%, SL = 4%");
  expect(result.text).toContain("только если цена не опустится ниже 101,00 USDT на момент входа");
  expect(result.text).toContain("Планируемое матожидание торговли: +0,32 R · матрица +0,21 R");
  expect(result.text).toContain("🧠 Алгоритм: Пользовательское имя\n🔬 Исследование №42");
  expect(result.text).not.toContain("0,01");
  expect(result.text).not.toContain("сетапов в день");
  expect(result.text).not.toContain("Цена входа:");
  expect(result.targetAt).toBe(at + 180);
});
it.each([
  ["short", -1, "не опустится ниже 99,00"], ["short", 0, "не опустится ниже 100,00"],
  ["long", 1, "не поднимется выше 99,00"], ["long", -1, "не поднимется выше 101,00"], ["long", 0, "не поднимется выше 100,00"],
] as const)("preserves the signed %s gate %s", (direction, priceChangePct, condition) => {
  expect(text({ direction, priceChangePct }).text).toContain(condition);
});
it("omits null conditions and zero-shift future-wait wording, without calling an entry confirmed", () => {
  const result = text({ timeShiftMinutes: 0, priceChangePct: null });
  expect(result.text).not.toContain("⏳"); expect(result.text).not.toContain("только если");
  expect(result.text).not.toContain("условие выполнено");
  expect(result.targetAt).toBe(at);
});
it("uses a stable target and configured timezone, marking elapsed targets rather than moving them forward", () => {
  applyInterfaceSettings({ timezone: "Europe/Moscow", dateOrder: "dmy" });
  const before = text({}, at + 60); const after = text({}, at + 600);
  expect(before.targetAt).toBe(after.targetAt);
  expect(before.text).toContain("22:46:00 Europe/Moscow");
  expect(after.entryState).toBe("elapsed");
  expect(after.text).toContain("рекомендуемое время уже прошло");
  expect(after.text).not.toContain("через 3 мин");
});
it("does not round decimal signal prices through binary floating point for the known threshold", () => {
  const result = formatSetupRecommendation({ ...setup, closePrice: "10000000000000000.01", recommendation: { ...recommendation, priceChangePct: 1 } }, at);
  expect(result?.kind === "ready" && result.text).toContain("10100000000000000,0101 USDT");
});
it("preserves snapshot name as plain text and never creates a recommendation for a legacy WATCH", () => {
  expect(text({ externalName: "<b>Снимок</b>" }).text).toContain("<b>Снимок</b>");
  expect(formatSetupRecommendation({ exchange: "bybit", symbol: "COINUSDT", closePrice: "100", closeTimeUnix: at }, at)).toBeNull();
});
it("reports an invalid snapshot rather than inventing future entry data", () => {
  expect(formatSetupRecommendation({ ...setup, recommendation: { ...recommendation, rawR: NaN } }, at)?.kind).toBe("invalid");
});
it("requires canonical rawR and rejects an unshipped alias instead of reconstructing the snapshot", () => {
  const { rawR, ...snapshot } = recommendation;
  const malformed = { ...snapshot, selectedCellRawR: rawR } as unknown as AdaptiveRecommendation;
  expect(formatSetupRecommendation({ ...setup, recommendation: malformed }, at)?.kind).toBe("invalid");
});
