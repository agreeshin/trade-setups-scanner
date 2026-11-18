import { effectiveTimezone } from "./datetime-format";

export type ExpectancyUnavailableReason = "missing_selected_cell" | "missing_selected_mean" | "missing_matrix_mean" | "missing_frozen_outcomes"
  | "unproven_outcome_provenance" | "incomplete_outcome_history" | "no_closed_outcomes" | "insufficient_finite_days";
export type ExpectancyMetric = { status: "available"; value: number } | { status: "unavailable"; reason: ExpectancyUnavailableReason };
export interface AdaptiveExpectancySnapshot {
  calculationVersion: number; timezone: "UTC"; studyId: number; generationId: number; slotId: number;
  derivedSnapshotId: string | null; sourceSnapshotHash: string; sourceCutoff: number;
  fromUnix: number; toUnix: number; periodDays: number;
  selectedMeanR: ExpectancyMetric; tpCount: number | null; slCount: number | null; closedCount: number | null;
  winRatePct: ExpectancyMetric; matrixMeanR: ExpectancyMetric; trendDeltaR: ExpectancyMetric; forecastR: ExpectancyMetric; forecastAtUnix: number;
  imageKey: null | { studyId: string; derivedSnapshotId: string; inputSnapshotHash: string; tpPct: number; slPct: number; calculationVersion: number; timezone: "UTC"; renderVersion: number };
}

/** Immutable server snapshot; never fill missing fields from today's algorithm or best study. */
export interface AdaptiveRecommendation {
  direction: "short" | "long";
  tpPct: number;
  slPct: number;
  rawR: number;
  meanR: number;
  robustScore: number;
  timeShiftMinutes: number;
  priceChangePct: number | null;
  externalName: string;
  studyId: number;
  generationId: number;
  slotId: number;
  ownerId: string;
  clusterEnabled: boolean;
  clusterGapMinutes: number | null;
  params: Record<string, string | number>;
  provenance: { algorithmId: string; algorithmVersion: string; optimizerVersion: string; seed: number; snapshotHash: string; capturedAt: string; fromUnix: number; toUnix: number; observationCutoff: number };
  rawSignalAt?: number;
  expectancy?: AdaptiveExpectancySnapshot | null;
}
interface RecommendationSetup {
  exchange: string; symbol: string; closePrice: string; closeTimeUnix: number;
  recommendation?: AdaptiveRecommendation | null;
}
export interface RecommendationSegment { style: "plain" | "bold" | "code"; text: string }
export type FormattedRecommendation = { kind: "ready"; text: string; segments: RecommendationSegment[]; targetAt: number; entryState: "future" | "now" | "elapsed" }
  | { kind: "invalid"; message: string };

interface Decimal { coefficient: bigint; scale: number }
function decimal(raw: string): Decimal | null {
  if (raw.length > 256) return null;
  const match = /^([+-]?)(\d+)(?:\.(\d*))?(?:e([+-]?\d+))?$/i.exec(raw);
  if (!match) return null;
  const exponent = Number(match[4] ?? 0);
  if (!Number.isSafeInteger(exponent) || Math.abs(exponent) > 324) return null;
  let coefficient = BigInt(`${match[1] === "-" ? "-" : ""}${match[2]}${match[3] ?? ""}`);
  let scale = (match[3]?.length ?? 0) - exponent;
  if (scale < 0) { coefficient *= 10n ** BigInt(-scale); scale = 0; }
  return { coefficient, scale };
}
function amount(value: Decimal): string {
  const negative = value.coefficient < 0n;
  const digits = (negative ? -value.coefficient : value.coefficient).toString().padStart(value.scale + 1, "0");
  const whole = value.scale ? digits.slice(0, -value.scale) : digits;
  const fraction = (value.scale ? digits.slice(-value.scale) : "").replace(/0+$/, "").padEnd(2, "0");
  return `${negative ? "-" : ""}${whole}.${fraction}`;
}
function threshold(price: Decimal, pct: number, direction: "short" | "long"): Decimal | null {
  const percent = decimal(String(pct));
  if (!percent) return null;
  const factor = 100n * 10n ** BigInt(percent.scale) + (direction === "short" ? percent.coefficient : -percent.coefficient);
  return { coefficient: price.coefficient * factor, scale: price.scale + percent.scale + 2 };
}
const numberFormat = new Intl.NumberFormat("en-US", { useGrouping: false, maximumFractionDigits: 12 });
const riskFormat = new Intl.NumberFormat("en-US", { useGrouping: false, minimumFractionDigits: 2, maximumFractionDigits: 2, signDisplay: "always" });
const winRateFormat = new Intl.NumberFormat("en-US", { useGrouping: false, maximumFractionDigits: 2 });
const unavailableReasons: Record<ExpectancyUnavailableReason, string> = {
  missing_selected_cell: "не сохранены счётчики выбранной пары", missing_selected_mean: "не сохранено среднее выбранной пары",
  missing_matrix_mean: "не сохранено среднее матрицы", missing_frozen_outcomes: "нет сохранённых исходных дневных результатов",
  unproven_outcome_provenance: "происхождение исходов не подтверждено", incomplete_outcome_history: "история исходов неполна",
  no_closed_outcomes: "нет закрытых TP/SL-сделок", insufficient_finite_days: "недостаточно дней с закрытыми сделками",
};
const record = (value: unknown): Record<string, unknown> | null => value !== null && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : null;
const boundedText = (value: unknown): value is string => typeof value === "string" && value.length > 0 && value.length <= 256 && !/\p{Cc}/u.test(value);
const unixTimestamp = (value: unknown): value is number => typeof value === "number" && Number.isSafeInteger(value) && Math.abs(value * 1000) <= 8.64e15;
const count = (value: unknown): value is number => typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
function metric(value: unknown): value is ExpectancyMetric {
  const item = record(value);
  return item !== null && (item.status === "available" ? typeof item.value === "number" && Number.isFinite(item.value)
    : item.status === "unavailable" && typeof item.reason === "string" && Object.hasOwn(unavailableReasons, item.reason));
}
function frozenPeriod(recommendation: AdaptiveRecommendation): number | null {
  const p = recommendation.provenance;
  return p && unixTimestamp(p.fromUnix) && unixTimestamp(p.toUnix) && p.toUnix > p.fromUnix && p.toUnix - p.fromUnix <= 3660 * 86400
    ? (p.toUnix - p.fromUnix) / 86400 : null;
}
/** Validate the scalar identity; never derive trend/forecast, load a profile or retain daily series. */
function validExpectancy(value: unknown, recommendation: AdaptiveRecommendation, signalAt: number): value is AdaptiveExpectancySnapshot {
  const item = record(value);
  const p = recommendation.provenance;
  const periodDays = frozenPeriod(recommendation);
  if (!item || !p || !unixTimestamp(signalAt) || item.calculationVersion !== 2 || item.timezone !== "UTC"
    || item.studyId !== recommendation.studyId || item.generationId !== recommendation.generationId || item.slotId !== recommendation.slotId
    || !Number.isSafeInteger(recommendation.slotId) || recommendation.slotId < 1
    || item.forecastAtUnix !== signalAt || (recommendation.rawSignalAt !== undefined && recommendation.rawSignalAt !== signalAt)
    || !unixTimestamp(item.sourceCutoff) || item.sourceCutoff !== p.observationCutoff || item.sourceCutoff > signalAt
    || !boundedText(item.sourceSnapshotHash) || item.sourceSnapshotHash !== p.snapshotHash
    || item.fromUnix !== p.fromUnix || item.toUnix !== p.toUnix || item.sourceCutoff > p.toUnix
    || periodDays === null || item.periodDays !== periodDays
    || (item.derivedSnapshotId !== null && !boundedText(item.derivedSnapshotId))
    || !metric(item.selectedMeanR) || item.selectedMeanR.status !== "available" || item.selectedMeanR.value !== recommendation.rawR
    || !metric(item.matrixMeanR) || item.matrixMeanR.status !== "available" || item.matrixMeanR.value !== recommendation.meanR
    || !metric(item.winRatePct) || !metric(item.trendDeltaR) || !metric(item.forecastR)) return false;
  const allCounts = count(item.tpCount) && count(item.slCount) && count(item.closedCount) && item.tpCount + item.slCount === item.closedCount;
  const unknownCounts = item.tpCount === null && item.slCount === null && item.closedCount === null;
  if (!allCounts && !(unknownCounts && item.winRatePct.status === "unavailable")) return false;
  if (item.winRatePct.status === "available" && (!allCounts || !count(item.closedCount) || item.closedCount === 0
    || item.winRatePct.value < 0 || item.winRatePct.value > 100 || !count(item.tpCount)
    || Math.abs(item.winRatePct.value - 100 * item.tpCount / item.closedCount) > 1e-9)) return false;
  const key = record(item.imageKey);
  if (item.imageKey !== null && (key?.studyId !== String(recommendation.studyId) || key.derivedSnapshotId !== item.derivedSnapshotId
    || !boundedText(key.derivedSnapshotId) || key.inputSnapshotHash !== p.snapshotHash || key.tpPct !== recommendation.tpPct || key.slPct !== recommendation.slPct
    || key.calculationVersion !== 2 || key.timezone !== "UTC" || key.renderVersion !== 1)) return false;
  return true;
}
const plain = (text: string): RecommendationSegment => ({ style: "plain", text });
const code = (text: string): RecommendationSegment => ({ style: "code", text });
const bold = (text: string): RecommendationSegment => ({ style: "bold", text });
const risk = (value: number): string => `${riskFormat.format(Number(value.toFixed(2)))} R`;

/** Presentation only: no polling, publication, entry-condition evaluation or live mutation. */
export function formatSetupRecommendation(setup: RecommendationSetup, nowUnix = Date.now() / 1000): FormattedRecommendation | null {
  const recommendation = setup.recommendation;
  if (recommendation === undefined || recommendation === null) return null;
  const invalid: FormattedRecommendation = { kind: "invalid", message: "Сохранённая рекомендация неполна или имеет неподдерживаемый формат. Будущие условия не восстановлены из текущих настроек." };
  const selectedCellR = recommendation.rawR;
  const price = decimal(setup.closePrice);
  if (!price || price.coefficient <= 0n || !Number.isFinite(setup.closeTimeUnix) || !Number.isFinite(nowUnix)
    || !["short", "long"].includes(recommendation.direction)
    || typeof selectedCellR !== "number" || ![recommendation.tpPct, recommendation.slPct, selectedCellR, recommendation.meanR].every(Number.isFinite)
    || recommendation.tpPct <= 0 || recommendation.tpPct > 100 || recommendation.slPct <= 0 || recommendation.slPct > 100
    || !Number.isSafeInteger(recommendation.timeShiftMinutes) || recommendation.timeShiftMinutes < 0 || recommendation.timeShiftMinutes > 43200
    || (recommendation.priceChangePct !== null && (!Number.isFinite(recommendation.priceChangePct) || recommendation.priceChangePct < -100 || recommendation.priceChangePct > 1000))
    || typeof recommendation.externalName !== "string" || recommendation.externalName.length > 240 || [...recommendation.externalName].length > 120 || recommendation.externalName.trim() === "" || /\p{Cc}/u.test(recommendation.externalName)
    || !Number.isSafeInteger(recommendation.studyId) || recommendation.studyId < 1
    || !Number.isSafeInteger(recommendation.generationId) || recommendation.generationId < 1) return invalid;
  const targetAt = setup.closeTimeUnix + recommendation.timeShiftMinutes * 60;
  if (!Number.isFinite(targetAt) || Math.abs(targetAt * 1000) > 8.64e15) return invalid;
  const entryState = nowUnix < targetAt ? "future" : nowUnix > targetAt ? "elapsed" : "now";
  const saved = recommendation.expectancy;
  const enrichment = validExpectancy(saved, recommendation, setup.closeTimeUnix) ? saved : null;
  const unavailable = saved === undefined || saved === null ? "дополнительные метрики не сохранены" : "снимок метрик некорректен или не относится к этому сетапу";
  const periodDays = enrichment?.periodDays ?? frozenPeriod(recommendation);
  const period = periodDays === null ? plain("недоступно") : code(numberFormat.format(periodDays));
  const metricSegment = (value: ExpectancyMetric | undefined, format = risk): RecommendationSegment => value?.status === "available" ? code(format(value.value))
    : plain(`недоступно (${value?.status === "unavailable" ? unavailableReasons[value.reason] : unavailable})`);
  const segments: RecommendationSegment[] = [];
  let firstLine = true;
  function line(...parts: (RecommendationSegment | string)[]) {
    if (!firstLine) segments.push(plain("\n"));
    firstLine = false;
    segments.push(...parts.map((part) => typeof part === "string" ? plain(part) : part));
  }
  line(`${recommendation.direction === "short" ? "🔴" : "🟢"} `, code(setup.symbol), ` · ${recommendation.direction.toUpperCase()} · ${setup.exchange === "bybit" ? "Bybit" : setup.exchange}`);
  line("Цена: ", code(amount(price)), " USDT");
  line(); line(bold("Условия сделки:"));
  if (recommendation.timeShiftMinutes > 0) {
    const timezone = effectiveTimezone();
    const clock = new Intl.DateTimeFormat("en-GB", { timeZone: timezone, hour: "2-digit", minute: "2-digit", second: "2-digit", hourCycle: "h23" }).format(new Date(targetAt * 1000));
    line("⏳ Рекомендуемое время входа: ", code(`${clock} ${timezone}`), " (", ...(entryState === "elapsed" ? ["рекомендуемое время уже прошло"]
      : ["через ", code(numberFormat.format(recommendation.timeShiftMinutes)), " мин после сетапа"]), ")");
  }
  line("🔖 TP = ", code(`${numberFormat.format(recommendation.tpPct)}%`), ", SL = ", code(`${numberFormat.format(recommendation.slPct)}%`), " (от фактической цены входа)");
  if (recommendation.priceChangePct !== null) {
    const bound = threshold(price, recommendation.priceChangePct, recommendation.direction);
    if (!bound) return invalid;
    line(`⚠️ Заходить в сделку, если цена будет ${recommendation.direction === "short" ? "выше" : "ниже"} `, code(amount(bound)), " USDT на момент входа");
  }
  line(); line(bold("Матожидание торговли:"));
  line("Прогноз МО торговли: ", metricSegment(enrichment?.forecastR));
  line("Среднее МО за последние ", period, " суток: ", code(risk(selectedCellR)));
  line("Динамика МО за последние ", period, " суток: ", metricSegment(enrichment?.trendDeltaR));
  line("Средний винрейт: ", metricSegment(enrichment?.winRatePct, (value) => `${winRateFormat.format(value)}%`));
  line("В среднем для других уровней TP и SL: ", code(risk(recommendation.meanR)));
  line(); line("Алгоритм: ", recommendation.externalName); line("Исследование №", code(String(recommendation.studyId)));
  return { kind: "ready", text: segments.map((part) => part.text).join(""), segments, targetAt, entryState };
}
