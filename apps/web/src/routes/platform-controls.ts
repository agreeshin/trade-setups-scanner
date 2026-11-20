import { effectiveTimezone, formatUnix, parseZonedDateTimeInput, zonedDateTimeInput, zonedDayStart } from "../lib/datetime-format";
import { apiFetch } from "../lib/platform-api";
import type { AdaptiveRecommendation } from "../lib/setup-recommendation";

export interface Algorithm {
  id: string;
  name: string;
  externalName?: string;
  version: string;
  description: string;
  enabled: boolean;
  params: Record<string, string | number>;
  fields: { key: string; label: string; type: "decimal" | "integer"; min: number; max: number; unit: string; optimization?: { enabledByDefault: boolean; values: (string | number)[] } }[];
}

export const algorithmDisplayName = (algorithm: Pick<Algorithm, "name" | "externalName">): string => algorithm.externalName ?? algorithm.name;

export interface SetupRow {
  id?: number;
  exchange: string;
  category?: string;
  symbol: string;
  algorithmId: string;
  externalName?: string | null;
  origin?: string;
  adaptiveSlotId?: number | null;
  adaptiveGenerationId?: number | null;
  recommendation?: AdaptiveRecommendation | null;
  algorithmVersion?: string;
  params?: Record<string, string | number>;
  details?: {
    growthPct: string | null;
    volumeLongRatio: string | null;
    volumeShortRatio: string | null;
    overLong: boolean;
    overShort: boolean;
    missingMinutes: number;
  } | null;
  openTimeUnix: number;
  closeTimeUnix: number;
  closePrice: string;
}

export const localTimezoneLabel = (): string => effectiveTimezone();
export const localTime = (unix: number): string => formatUnix(unix);

export function localDateInput(unix: number): string {
  return zonedDateTimeInput(unix);
}

/** Legacy export name: a changed date now resets to midnight in the configured timezone. */
export function utcMidnightOnDateChange(previous: string, next: string): string {
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(next) || previous.slice(0, 10) === next.slice(0, 10)) return next;
  const picked = parseZonedDateTimeInput(`${next.slice(0, 10)}T12:00`);
  return Number.isFinite(picked) ? zonedDateTimeInput(zonedDayStart(picked)) : next;
}

/** Optimizer end date: today uses current configured-zone time; other date changes keep their time. */
export function nowOnTodayDateChange(previous: string, next: string): string {
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(next) || previous.slice(0, 10) === next.slice(0, 10)) return next;
  const now = localDateInput(Date.now() / 1000);
  return next.slice(0, 10) === now.slice(0, 10) ? now : next;
}

/** End date change: today uses current configured-zone time; other dates use local midnight. */
export function endOfRangeDateChange(previous: string, next: string): string {
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(next) || previous.slice(0, 10) === next.slice(0, 10)) return next;
  const now = localDateInput(Date.now() / 1000);
  return next.slice(0, 10) === now.slice(0, 10) ? now : utcMidnightOnDateChange(previous, next);
}

export function replayRange(from: string, to: string): { fromUnix: number; toUnix: number } {
  const fromUnix = parseZonedDateTimeInput(from);
  const toUnix = parseZonedDateTimeInput(to);
  if (!Number.isFinite(fromUnix) || !Number.isFinite(toUnix) || toUnix <= fromUnix) {
    throw new Error("Укажите корректные даты: конец диапазона должен быть позже начала.");
  }
  return { fromUnix, toUnix };
}

export async function writePlatform(path: string, body: unknown, method = "PUT"): Promise<void> {
  const response = await apiFetch(path, { method, headers: { "content-type": "application/json" }, body: JSON.stringify(body) });
  if (response.ok) return;
  if (response.status === 429) {
    const detail = await response.json().catch(() => null) as {
      resource?: string; requested?: number; limit?: number;
    } | null;
    if (detail?.resource && Number.isFinite(detail.requested) && Number.isFinite(detail.limit)) {
      const labels: Record<string, string> = {
        queue_jobs: "очередь задач",
        work_minutes: "объём обработки",
        database_bytes: "бюджет базы данных",
      };
      throw new Error(`Операция отклонена: ${labels[detail.resource] ?? detail.resource} (${detail.requested} > ${detail.limit}). Уменьшите диапазон или повторите позже.`);
    }
  }
  throw new Error(`Не удалось сохранить: HTTP ${response.status}`);
}

export async function savePreference(key: string, value: unknown): Promise<void> {
  const response = await apiFetch("/v1/settings/ui");
  if (!response.ok) throw new Error(`Настройки интерфейса недоступны: HTTP ${response.status}`);
  const current = await response.json() as { value: Record<string, unknown> };
  await writePlatform("/v1/settings/ui", { value: { ...current.value, [key]: value } });
}

export function errorText(error: unknown): string {
  return error instanceof Error ? error.message : "Ошибка соединения";
}
