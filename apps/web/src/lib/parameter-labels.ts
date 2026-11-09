/** Human-readable labels for algorithm launch parameters, shared by runs and analytics views. */
export const parameterLabels: Record<string, string> = {
  priceGrowthPct: "Рост цены (%)", windowMinutes: "Окно (мин)", volumeMultiplier: "Множитель объёма (×)",
  baselineLongDays: "Длинная база (дни)", baselineShortDays: "Короткая база (дни)", cooldownMinutes: "Пауза между сигналами (мин)",
  maxGapPercent: "Допустимые пропуски (%)", maxConsecutiveGapMinutes: "Пропуски подряд (мин)", requiredRecentMinutes: "Непрерывная недавняя история (мин)",
};
