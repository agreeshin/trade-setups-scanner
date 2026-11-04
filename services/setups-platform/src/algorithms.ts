import { Type } from "@sinclair/typebox";
import type { Kysely } from "kysely";
import type { SignalsDB } from "./db/schema.js";
import { DEFAULT_SPIKE_PARAMS, SPIKE_ALGORITHM_ID, SPIKE_ALGORITHM_VERSION, type SpikeParams } from "./detector/price-volume-spike.js";
import { getSetting, setSetting } from "./settings.js";

const decimal = (maximum: number) => Type.String({ pattern: "^(0|[1-9][0-9]{0,3})(\\.[0-9]{1,6})?$", maxLength: 12, description: `0..${maximum}` });
export const SpikeParamsSchema = Type.Object({
  priceGrowthPct: decimal(1000),
  windowMinutes: Type.Integer({ minimum: 1, maximum: 1440 }),
  baselineLongDays: Type.Integer({ minimum: 1, maximum: 90 }),
  baselineShortDays: Type.Integer({ minimum: 1, maximum: 90 }),
  volumeMultiplier: decimal(1000),
  cooldownMinutes: Type.Integer({ minimum: 0, maximum: 10080 }),
  maxGapPercent: decimal(100),
  maxConsecutiveGapMinutes: Type.Integer({ minimum: 0, maximum: 1440 }),
  requiredRecentMinutes: Type.Integer({ minimum: 0, maximum: 1440 }),
}, { additionalProperties: false });

export function validateSpikeParams(params: SpikeParams): string | undefined {
  if (Number(params.priceGrowthPct) > 1000 || Number(params.volumeMultiplier) <= 0 || Number(params.volumeMultiplier) > 1000 || Number(params.maxGapPercent) > 100) return "Порог вне допустимого диапазона";
  if (params.baselineShortDays > params.baselineLongDays) return "Короткая база не должна превышать длинную";
  if (params.windowMinutes >= params.baselineShortDays * 1440) return "Окно роста должно быть короче базового окна";
  return undefined;
}

const fields = [
  { key: "priceGrowthPct", label: "Рост цены", type: "decimal", min: 0, max: 1000, unit: "%" },
  { key: "windowMinutes", label: "Окно роста цены и оборота", type: "integer", min: 1, max: 1440, unit: "мин" },
  { key: "volumeMultiplier", label: "Множитель оборота", type: "decimal", min: 0.000001, max: 1000, unit: "×" },
  { key: "baselineLongDays", label: "Длинная база", type: "integer", min: 1, max: 90, unit: "дней" },
  { key: "baselineShortDays", label: "Короткая база", type: "integer", min: 1, max: 90, unit: "дней" },
  { key: "cooldownMinutes", label: "Интервал между сетапами", type: "integer", min: 0, max: 10080, unit: "мин" },
  { key: "maxGapPercent", label: "Допустимая доля пропусков", type: "decimal", min: 0, max: 100, unit: "%" },
  { key: "maxConsecutiveGapMinutes", label: "Максимальный непрерывный пропуск", type: "integer", min: 0, max: 1440, unit: "мин" },
  { key: "requiredRecentMinutes", label: "Последние минуты без пропусков", type: "integer", min: 0, max: 1440, unit: "мин" },
];

export async function getAlgorithm(db: Kysely<SignalsDB>, id = SPIKE_ALGORITHM_ID) {
  if (id !== SPIKE_ALGORITHM_ID) return undefined;
  const stored = await getSetting(db, `algorithm:${id}`);
  return {
    id, name: "Всплеск цены и оборота", version: SPIKE_ALGORITHM_VERSION,
    description: "Нейтральный WATCH: рост цены и оборот выше короткой или длинной базы. Общая конфигурация для истории и live.",
    params: { ...DEFAULT_SPIKE_PARAMS, ...(stored.params as Partial<SpikeParams> | undefined) },
    enabled: stored.enabled !== false,
    fields,
  };
}

export async function saveAlgorithm(db: Kysely<SignalsDB>, id: string, params: SpikeParams, enabled: boolean): Promise<void> {
  await setSetting(db, `algorithm:${id}`, { params, enabled });
}
