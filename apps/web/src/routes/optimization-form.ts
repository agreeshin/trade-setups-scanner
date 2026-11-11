import type { OptimizationAnalytics, OptimizationSpec, OptimizationValue } from "../lib/optimization-api";
import type { Algorithm } from "./platform-controls";
import { replayRange } from "./platform-controls";
import type { CatalogInstrument } from "./instrument-picker";

export interface SearchField {
  key: string; label: string; type: "decimal" | "integer" | "boolean" | "nullable";
  min: number; max: number; unit: string;
  optimization?: { enabledByDefault: boolean; values: OptimizationValue[] };
}
export interface DomainDraft { baseline: string; search: boolean; values: string }
export type DomainDrafts = Record<string, DomainDraft>;
export const ANALYTICS_SEARCH_FIELDS: SearchField[] = [
  { key: "clusterEnabled", label: "Кластеризация", type: "boolean", min: 0, max: 1, unit: "", optimization: { enabledByDefault: true, values: [false, true] } },
  { key: "clusterGapMinutes", label: "Разрыв кластера, мин", type: "integer", min: 1, max: 10080, unit: "мин", optimization: { enabledByDefault: true, values: [240, 480] } },
  { key: "timeShiftMinutes", label: "Time-shift, мин", type: "integer", min: 0, max: 43200, unit: "мин", optimization: { enabledByDefault: true, values: [0, 5] } },
  { key: "priceChangePct", label: "Гейт цены, %", type: "nullable", min: -100, max: 1000, unit: "%", optimization: { enabledByDefault: true, values: [null, 0, 1] } },
];
export const ANALYTICS_BASELINE: OptimizationAnalytics = { clusterEnabled: true, clusterGapMinutes: 480, timeShiftMinutes: 0, priceChangePct: null };
export const DEFAULT_AXIS = "4, 6, 8, 10, 12, 14, 16, 18, 20";
// Bounds the form's retained text and expanded search domains, not the server's search algorithm.
const MAX_DOMAIN_VALUES = 256;
export function initialDomains(fields: SearchField[], params: Record<string, OptimizationValue>): DomainDrafts {
  return Object.fromEntries(fields.map((field) => [field.key, {
    baseline: String(params[field.key] ?? "null"), search: field.optimization?.enabledByDefault ?? false,
    values: (field.optimization?.values ?? [params[field.key] ?? null]).map(String).join(", "),
  }]));
}
function fieldValue(field: SearchField, raw: string, algorithm: boolean): OptimizationValue {
  const text = raw.trim();
  if (field.type === "boolean" && (text === "true" || text === "false")) return text === "true";
  if (field.type === "nullable" && text === "null") return null;
  const value = Number(text);
  if (!text || field.type === "boolean" || !Number.isFinite(value) || value < field.min || value > field.max || (field.type === "integer" && !Number.isSafeInteger(value))) {
    throw new Error(`Проверьте «${field.label}»: ${field.type === "boolean" ? "true или false" : `от ${field.min} до ${field.max}${field.type === "integer" ? ", целое число" : ""}${field.type === "nullable" ? " или null (гейт отключён)" : ""}`}.`);
  }
  if (algorithm && field.type === "decimal") {
    if (!/^[+-]?(?:\d+(?:\.\d*)?|\.\d+)$/.test(text)) throw new Error(`«${field.label}»: используйте десятичную запись с точкой.`);
    return text;
  }
  return value;
}
function parseDomains<T extends OptimizationValue>(fields: SearchField[], drafts: DomainDrafts, algorithm: boolean, typed: (value: OptimizationValue) => T) {
  const baseline: Record<string, T> = {};
  const search: Record<string, T[]> = {};
  for (const field of fields) {
    const draft = drafts[field.key];
    if (!draft) throw new Error(`Не заполнено поле «${field.label}».`);
    baseline[field.key] = typed(fieldValue(field, draft.baseline, algorithm));
    if (!draft.search) continue;
    const parts = draft.values.split(",");
    if (draft.values.length > 8192 || parts.length > MAX_DOMAIN_VALUES) throw new Error(`«${field.label}»: не больше ${MAX_DOMAIN_VALUES} значений в домене.`);
    const values = parts.map((part) => typed(fieldValue(field, part, algorithm)));
    const seen = new Set<string>();
    search[field.key] = values.filter((value) => {
      // Decimal strings are exact detector thresholds. Never merge them via IEEE-754;
      // the server can canonicalize textually different but exactly equal decimals.
      const key = `${typeof value}:${String(value)}`;
      if (seen.has(key)) return false;
      seen.add(key); return true;
    });
  }
  return { baseline, search };
}
function algorithmValue(value: OptimizationValue): string | number {
  if (typeof value !== "number" && typeof value !== "string") throw new Error("Метаданные алгоритма должны задавать числовой параметр.");
  return value;
}
function booleanValue(value: OptimizationValue | undefined): boolean {
  if (typeof value !== "boolean") throw new Error("Кластеризация: требуется true или false.");
  return value;
}
function numberValue(value: OptimizationValue | undefined): number {
  if (typeof value !== "number") throw new Error("Требуется числовое значение аналитики.");
  return value;
}
function nullableValue(value: OptimizationValue | undefined): number | null {
  return value === null ? null : numberValue(value);
}
export function parseAxis(raw: string, name: string): number[] {
  const parts = raw.split(",");
  const values = parts.map((part) => part.trim() ? Number(part.trim()) : NaN);
  if (raw.length > 8192 || values.length > 64 || values.some((value) => !Number.isFinite(value) || value <= 0 || value > 100) || new Set(values).size !== values.length) {
    throw new Error(`${name}: укажите от 1 до 64 уникальных конечных уровней больше 0 и не больше 100 через запятую.`);
  }
  return values;
}
export function integerInput(raw: string, name: string, min: number, max = Number.MAX_SAFE_INTEGER): number {
  const value = Number(raw);
  if (!raw.trim() || !Number.isSafeInteger(value) || value < min || value > max) throw new Error(`${name}: требуется целое число от ${min} до ${max}.`);
  return value;
}
export interface ScopeDraft {
  from: string; to: string; symbols: string; direction: "short" | "long";
  setupMaturity: string; horizonMinutes: string; tpLevels: string; slLevels: string;
  minClosedPerCell: string; seed: string; maxTrials: string; maxSeconds: string;
}
export function buildOptimizationSpec(algorithm: Algorithm, scope: ScopeDraft, catalog: CatalogInstrument[], params: DomainDrafts, analytics: DomainDrafts): OptimizationSpec {
  const range = replayRange(scope.from, scope.to);
  if (range.fromUnix % 60 !== 0 || range.toUnix % 60 !== 0) throw new Error("Границы периода должны совпадать с минутной сеткой.");
  const symbols = [...new Set(scope.symbols.split(/[\s,;]+/).filter(Boolean))];
  if (!symbols.length) throw new Error("Выберите хотя бы один исторический инструмент.");
  const instruments = symbols.map((symbol) => {
    const instrument = catalog.find((item) => item.symbol === symbol);
    if (!instrument) throw new Error(`Инструмент ${symbol} отсутствует в historical-каталоге. Уберите его или обновите каталог.`);
    return { exchange: instrument.exchange, category: instrument.category, symbol: instrument.symbol };
  });
  const algorithmValues = parseDomains(algorithm.fields, params, true, algorithmValue);
  const analyticsValues = parseDomains(ANALYTICS_SEARCH_FIELDS, analytics, false, (value) => value);
  const analyticsParams: OptimizationAnalytics = {
    clusterEnabled: booleanValue(analyticsValues.baseline.clusterEnabled),
    clusterGapMinutes: numberValue(analyticsValues.baseline.clusterGapMinutes),
    timeShiftMinutes: numberValue(analyticsValues.baseline.timeShiftMinutes),
    priceChangePct: nullableValue(analyticsValues.baseline.priceChangePct),
  };
  const analyticsSearch: OptimizationSpec["analyticsSearch"] = {};
  if (analyticsValues.search.clusterEnabled) analyticsSearch.clusterEnabled = analyticsValues.search.clusterEnabled.map(booleanValue);
  if (analyticsValues.search.clusterGapMinutes) analyticsSearch.clusterGapMinutes = analyticsValues.search.clusterGapMinutes.map(numberValue);
  if (analyticsValues.search.timeShiftMinutes) analyticsSearch.timeShiftMinutes = analyticsValues.search.timeShiftMinutes.map(numberValue);
  if (analyticsValues.search.priceChangePct) analyticsSearch.priceChangePct = analyticsValues.search.priceChangePct.map(nullableValue);
  const maturity = Number(scope.setupMaturity);
  if (!scope.setupMaturity.trim() || !Number.isFinite(maturity) || maturity < 0 || maturity > 1 || Math.abs(maturity * 10 - Math.round(maturity * 10)) > 1e-8) throw new Error("Зрелость: число от 0 до 1 с шагом 0.1, фиксированное для исследования.");
  return {
    algorithmId: algorithm.id, algorithmVersion: algorithm.version,
    algorithmParams: { ...algorithm.params, ...algorithmValues.baseline }, algorithmSearch: algorithmValues.search,
    analyticsParams, analyticsSearch,
    instruments, ...range, direction: scope.direction, setupMaturity: maturity,
    horizonMinutes: integerInput(scope.horizonMinutes, "Горизонт", 1, 43200),
    tpLevels: parseAxis(scope.tpLevels, "TP"), slLevels: parseAxis(scope.slLevels, "SL"),
    minClosedPerCell: integerInput(scope.minClosedPerCell, "Минимум закрытых на ячейку", 1),
    seed: integerInput(scope.seed, "Seed", 0),
    maxTrials: integerInput(scope.maxTrials, "Бюджет trials", 1, 4096), maxSeconds: integerInput(scope.maxSeconds, "Бюджет времени", 1, 86400),
  };
}
