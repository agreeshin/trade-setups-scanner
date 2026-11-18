import { afterEach, beforeEach, expect, it } from "vitest";
import { applyInterfaceSettings } from "./datetime-format";
import { formatSetupRecommendation, type AdaptiveExpectancySnapshot, type AdaptiveRecommendation } from "./setup-recommendation";

const at = Date.parse("2026-11-12T19:43:00Z") / 1000;
const recommendation: AdaptiveRecommendation = { direction: "short", tpPct: 14, slPct: 4, rawR: 0.32, meanR: 0.21, robustScore: 0.01,
  timeShiftMinutes: 3, priceChangePct: 1, externalName: "Пользовательское имя", studyId: 42, generationId: 7, slotId: 5, ownerId: "admin", clusterEnabled: true, clusterGapMinutes: 480,
  params: { priceGrowthPct: "12", windowMinutes: 5, baselineLongDays: 30, baselineShortDays: 7, volumeMultiplier: "3", cooldownMinutes: 5, maxGapPercent: "1", maxConsecutiveGapMinutes: 5, requiredRecentMinutes: 10 },
  provenance: { algorithmId: "price_volume_spike", algorithmVersion: "1.1.1", optimizerVersion: "continuous-v1", seed: 42, snapshotHash: "frozen", capturedAt: "2026-11-12T19:43:00Z", fromUnix: at - 86400, toUnix: at, observationCutoff: at } };
const snapshot: AdaptiveExpectancySnapshot = {
  calculationVersion: 2, timezone: "UTC", studyId: 42, generationId: 7, slotId: 5, derivedSnapshotId: "derived-42", sourceSnapshotHash: "frozen", sourceCutoff: at,
  fromUnix: at - 86400, toUnix: at, periodDays: 1, selectedMeanR: { status: "available", value: 0.32 }, tpCount: 5, slCount: 3, closedCount: 8,
  winRatePct: { status: "available", value: 62.5 }, matrixMeanR: { status: "available", value: 0.21 }, trendDeltaR: { status: "available", value: -0.12 },
  forecastR: { status: "available", value: 0.44 }, forecastAtUnix: at, imageKey: null,
};
const setup = { exchange: "bybit", symbol: "COINUSDT", closeTimeUnix: at, closePrice: "100", recommendation };
beforeEach(() => { applyInterfaceSettings({ timezone: "UTC" }); });
afterEach(() => { applyInterfaceSettings({ timezone: "auto" }); });
function text(patch: Partial<AdaptiveRecommendation> = {}, now = at) {
  const result = formatSetupRecommendation({ ...setup, recommendation: { ...recommendation, ...patch } }, now);
  if (result?.kind !== "ready") throw new Error("Expected ready recommendation");
  return result;
}
it("formats the exact approved template order, saved scalar meanings, two headings and code values with decimal dots", () => {
  const result = text({ expectancy: snapshot });
  expect(result.text).toBe([
    "🔴 COINUSDT · SHORT · Bybit", "Цена: 100.00 USDT", "", "Условия сделки:",
    "⏳ Рекомендуемое время входа: 19:46:00 UTC (через 3 мин после сетапа)",
    "🔖 TP = 14%, SL = 4% (от фактической цены входа)",
    "⚠️ Заходить в сделку, если цена будет выше 101.00 USDT на момент входа", "",
    "Матожидание торговли:", "Прогноз МО торговли: +0.44 R", "Среднее МО за последние 1 суток: +0.32 R", "Динамика МО за последние 1 суток: -0.12 R",
    "Средний винрейт: 62.5%", "В среднем для других уровней TP и SL: +0.21 R", "", "Алгоритм: Пользовательское имя", "Исследование №42 от 12.11.2026",
  ].join("\n"));
  expect(result.segments.filter((part) => part.style === "bold").map((part) => part.text)).toEqual(["Условия сделки:", "Матожидание торговли:"]);
  expect(result.segments.find((part) => part.text === "Прогноз МО торговли: ")?.style).toBe("plain");
  expect(result.segments.filter((part) => part.style === "code").map((part) => part.text)).toEqual(["COINUSDT", "100.00", "19:46:00 UTC", "3", "14%", "4%", "101.00", "+0.44 R", "1", "+0.32 R", "1", "-0.12 R", "62.5%", "+0.21 R"]);
  expect(result.segments.filter((part) => part.style !== "code" && !part.text.includes("Исследование №")).some((part) => /\d/.test(part.text))).toBe(false);
  expect(result.segments.map((part) => part.text).join("")).toBe(result.text);
  expect(result.text).not.toMatch(/🧠|🔬|✅|сетапов в день|Цена входа:|0,32/);
  expect(result.targetAt).toBe(at + 180);
});

it.each([
  ["short", -1, "будет выше 99.00"], ["short", 0, "будет выше 100.00"],
  ["long", 1, "будет ниже 99.00"], ["long", -1, "будет ниже 101.00"], ["long", 0, "будет ниже 100.00"],
] as const)("preserves the exact signed %s gate %s", (direction, priceChangePct, condition) => {
  expect(text({ direction, priceChangePct }).text).toContain(condition);
});
it("omits null conditions and zero-shift future-wait wording, without calling an entry confirmed", () => {
  const result = text({ timeShiftMinutes: 0, priceChangePct: null });
  expect(result.text).not.toContain("⏳"); expect(result.text).not.toContain("только если");
  expect(result.text).not.toContain("условие выполнено"); expect(result.text).not.toContain("через 0");
  expect(result.targetAt).toBe(at);
});
it("uses a stable time-only target and explicit configured timezone without re-evaluating a saved forecast", () => {
  applyInterfaceSettings({ timezone: "Europe/Moscow", dateOrder: "dmy", time24: false });
  const before = text({ expectancy: snapshot }, at + 60); const after = text({ expectancy: snapshot }, at + 600);
  expect(before.targetAt).toBe(after.targetAt);
  expect(before.text).toContain("22:46:00 Europe/Moscow");
  expect(after.entryState).toBe("elapsed"); expect(after.text).toContain("рекомендуемое время уже прошло");
  expect(after.text).not.toContain("через 3 мин");
  expect(before.text.split("\n").find((line) => line.includes("Рекомендуемое время входа")) ?? "").not.toContain("2026"); expect(before.text).not.toContain("AM");
  expect(before.text).toContain("Прогноз МО торговли: +0.44 R"); expect(after.text).toContain("Прогноз МО торговли: +0.44 R");
});
it("omits the date in the entry time even when the recommended target crosses UTC midnight", () => {
  const midnightSignal = Date.parse("2026-11-12T23:59:00Z") / 1000;
  const result = formatSetupRecommendation({ ...setup, closeTimeUnix: midnightSignal }, midnightSignal);
  const entryLine = result?.kind === "ready" ? result.text.split("\n").find((line) => line.includes("Рекомендуемое время входа")) ?? "" : "";
  expect(entryLine).toContain("00:02:00 UTC");
  expect(entryLine).not.toMatch(/2026|13\.11|12\.11/);
});
it("does not round decimal signal prices through binary floating point for the known threshold", () => {
  const result = formatSetupRecommendation({ ...setup, closePrice: "10000000000000000.01", recommendation: { ...recommendation, priceChangePct: 1 } }, at);
  expect(result?.kind === "ready" && result.text).toContain("10100000000000000.0101 USDT");
});
it("preserves snapshot name/symbol as literal text and never creates a recommendation for a legacy WATCH", () => {
  const result = formatSetupRecommendation({ ...setup, symbol: "<CODE&USDT>", recommendation: { ...recommendation, externalName: "<b>Снимок</b>" } }, at);
  expect(result?.kind === "ready" && result.text).toContain("<b>Снимок</b>");
  expect(result?.kind === "ready" && result.segments.find((part) => part.text === "<CODE&USDT>" && part.style === "code")).toBeTruthy();
  expect(formatSetupRecommendation({ exchange: "bybit", symbol: "COINUSDT", closePrice: "100", closeTimeUnix: at }, at)).toBeNull();
});
it("reports an invalid base snapshot rather than inventing future entry data", () => {
  expect(formatSetupRecommendation({ ...setup, recommendation: { ...recommendation, rawR: NaN } }, at)?.kind).toBe("invalid");
});
it("requires canonical rawR and rejects an unshipped alias instead of reconstructing the snapshot", () => {
  const { rawR, ...saved } = recommendation;
  const malformed = { ...saved, selectedCellRawR: rawR } as unknown as AdaptiveRecommendation;
  expect(formatSetupRecommendation({ ...setup, recommendation: malformed }, at)?.kind).toBe("invalid");
});

it.each([undefined, null])("keeps legacy known trade conditions/rawR/matrix mean but explicitly marks missing enrichment %s unavailable", (expectancy) => {
  const before = structuredClone(recommendation);
  const result = text(expectancy === undefined ? {} : { expectancy });
  expect(result.text).toContain("Среднее МО за последние 1 суток: +0.32 R");
  expect(result.text).toContain("В среднем для других уровней TP и SL: +0.21 R");
  for (const label of ["Прогноз МО торговли", "Средний винрейт"]) expect(result.text).toContain(`${label}: недоступно (дополнительные метрики не сохранены)`);
  expect(result.text).toContain("Динамика МО за последние 1 суток: недоступно");
  expect(result.text).not.toContain("Средний винрейт: 0");
  expect(recommendation).toEqual(before); expect(recommendation).not.toHaveProperty("expectancy");
});
it("uses a provided fractional period only when it agrees with the original frozen study bounds", () => {
  const fromUnix = at - 1.5 * 86400;
  const result = text({ provenance: { ...recommendation.provenance, fromUnix }, expectancy: { ...snapshot, fromUnix, periodDays: 1.5 } });
  expect(result.text).toContain("Среднее МО за последние 1.5 суток: +0.32 R");
  expect(result.text).toContain("Динамика МО за последние 1.5 суток: -0.12 R");
});
it("renders independently unavailable metrics with their safe reason instead of zeroes", () => {
  const result = text({ expectancy: { ...snapshot, tpCount: null, slCount: null, closedCount: null,
    winRatePct: { status: "unavailable", reason: "missing_selected_cell" }, trendDeltaR: { status: "unavailable", reason: "insufficient_finite_days" },
    forecastR: { status: "unavailable", reason: "missing_frozen_outcomes" } } });
  expect(result.text).toContain("Прогноз МО торговли: недоступно (нет сохранённых исходных дневных результатов)");
  expect(result.text).toContain("Динамика МО за последние 1 суток: недоступно (недостаточно дней с закрытыми сделками)");
  expect(result.text).toContain("Средний винрейт: недоступно (не сохранены счётчики выбранной пары)");
  expect(result.text).toContain("Среднее МО за последние 1 суток: +0.32 R");
});
it("accepts zero as an available frozen value rather than confusing it with a missing metric", () => {
  const result = text({ expectancy: { ...snapshot, tpCount: 0, slCount: 8, winRatePct: { status: "available", value: 0 }, forecastR: { status: "available", value: 0 }, trendDeltaR: { status: "available", value: 0 } } });
  expect(result.text).toContain("Прогноз МО торговли: +0.00 R"); expect(result.text).toContain("Средний винрейт: 0%");
  expect(result.text).toContain("Динамика МО за последние 1 суток: +0.00 R");
});

const invalidPatches: Record<string, unknown>[] = [
  { calculationVersion: 1 }, { timezone: "auto" }, { studyId: 99 }, { generationId: 8 }, { slotId: 6 },
  { sourceSnapshotHash: "another-study" }, { sourceCutoff: at - 1 }, { sourceCutoff: at + 60 }, { forecastAtUnix: at + 180 },
  { fromUnix: at - 172800 }, { toUnix: at - 60 }, { periodDays: 7 }, { derivedSnapshotId: "" },
  { selectedMeanR: { status: "available", value: 99 } }, { matrixMeanR: { status: "available", value: 88 } },
  { forecastR: { status: "available", value: Infinity } }, { trendDeltaR: { status: "available", value: NaN } },
  { winRatePct: { status: "available", value: 101 } }, { winRatePct: { status: "available", value: 50 } },
  { forecastR: { status: "unavailable", reason: "RAW_PRIVATE_REASON" } }, { trendDeltaR: { status: "pending", value: 0.44 } },
  { tpCount: -1 }, { tpCount: 0.5 }, { slCount: null }, { closedCount: 100 }, { closedCount: Number.MAX_SAFE_INTEGER + 1 },
  { imageKey: { studyId: "42", derivedSnapshotId: "wrong", inputSnapshotHash: "frozen", tpPct: 14, slPct: 4, calculationVersion: 2, timezone: "UTC", renderVersion: 1 } },
];
it.each(invalidPatches.map((patch) => ({ patch })))("rejects malformed/mismatched optional enrichment without losing known historical conditions: $patch", ({ patch }) => {
  const expectancy = { ...snapshot, ...patch };
  const result = text({ expectancy });
  expect(result.text).toContain("Прогноз МО торговли: недоступно (снимок метрик некорректен или не относится к этому сетапу)");
  expect(result.text).toContain("Средний винрейт: недоступно"); expect(result.text).not.toMatch(/\+0\.44 R|-0\.12 R|62\.5%|RAW_PRIVATE_REASON|Infinity|NaN/);
  expect(result.text).toContain("Среднее МО за последние 1 суток: +0.32 R"); expect(result.text).toContain("В среднем для других уровней TP и SL: +0.21 R");
  expect(result.text).toContain("101.00 USDT на момент входа"); expect(result.text).toContain("Исследование №42");
});
it("checks the optional rawSignalAt identity and does not replace it with target time", () => {
  expect(text({ rawSignalAt: at, expectancy: snapshot }).text).toContain("Прогноз МО торговли: +0.44 R");
  expect(text({ rawSignalAt: at + 180, expectancy: snapshot }).text).toContain("Прогноз МО торговли: недоступно");
});
it("accepts a matching image identity only as metadata without embedding or fetching the asset", () => {
  const result = text({ expectancy: { ...snapshot, imageKey: { studyId: "42", derivedSnapshotId: "derived-42", inputSnapshotHash: "frozen", tpPct: 14, slPct: 4, calculationVersion: 2, timezone: "UTC", renderVersion: 1 } } });
  expect(result.text).toContain("Прогноз МО торговли: +0.44 R"); expect(result.text).not.toContain("imageKey"); expect(result.text).not.toContain("derived-42");
});
