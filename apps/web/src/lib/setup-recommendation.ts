import { effectiveTimezone, formatUnix } from "./datetime-format";

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
}
interface RecommendationSetup {
  exchange: string; symbol: string; closePrice: string; closeTimeUnix: number;
  recommendation?: AdaptiveRecommendation | null;
}
export type FormattedRecommendation = { kind: "ready"; text: string; targetAt: number; entryState: "future" | "now" | "elapsed" }
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
  return `${negative ? "-" : ""}${whole},${fraction}`;
}
function threshold(price: Decimal, pct: number, direction: "short" | "long"): Decimal | null {
  const percent = decimal(String(pct));
  if (!percent) return null;
  const factor = 100n * 10n ** BigInt(percent.scale) + (direction === "short" ? percent.coefficient : -percent.coefficient);
  return { coefficient: price.coefficient * factor, scale: price.scale + percent.scale + 2 };
}
const percentFormat = new Intl.NumberFormat("ru-RU", { useGrouping: false, maximumFractionDigits: 12 });
const riskFormat = new Intl.NumberFormat("ru-RU", { useGrouping: false, minimumFractionDigits: 2, maximumFractionDigits: 2, signDisplay: "always" });

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
  const lines = [
    `${recommendation.direction === "short" ? "🔴" : "🟢"} ${setup.symbol} · ${recommendation.direction.toUpperCase()} · ${setup.exchange === "bybit" ? "Bybit" : setup.exchange}`,
    `Цена: ${amount(price)} USDT`, "", "Условия сделки:",
  ];
  if (recommendation.timeShiftMinutes > 0) lines.push(`⏳ Рекомендуемое время входа: ${formatUnix(targetAt)} ${effectiveTimezone()} (${entryState === "elapsed" ? "рекомендуемое время уже прошло" : `через ${recommendation.timeShiftMinutes} мин после сигнала`})`);
  lines.push(`🔖 TP = ${percentFormat.format(recommendation.tpPct)}%, SL = ${percentFormat.format(recommendation.slPct)}%`);
  if (recommendation.priceChangePct !== null) {
    const bound = threshold(price, recommendation.priceChangePct, recommendation.direction);
    if (!bound) return invalid;
    lines.push(`✅ Заходить в сделку, только если цена не ${recommendation.direction === "short" ? "опустится ниже" : "поднимется выше"} ${amount(bound)} USDT на момент входа`);
  }
  lines.push("", `Планируемое матожидание торговли: ${riskFormat.format(selectedCellR)} R · матрица ${riskFormat.format(recommendation.meanR)} R`,
    "", `🧠 Алгоритм: ${recommendation.externalName}`, `🔬 Исследование №${recommendation.studyId}`);
  return { kind: "ready", text: lines.join("\n"), targetAt, entryState };
}
