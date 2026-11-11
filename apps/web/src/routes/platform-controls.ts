import { effectiveTimezone, formatUnix } from "../lib/datetime-format";
import { apiFetch } from "../lib/platform-api";

export interface Algorithm {
  id: string;
  name: string;
  version: string;
  description: string;
  enabled: boolean;
  params: Record<string, string | number>;
  fields: { key: string; label: string; type: "decimal" | "integer"; min: number; max: number; unit: string; optimization?: { enabledByDefault: boolean; values: (string | number)[] } }[];
}

export interface SetupRow {
  exchange: string;
  category?: string;
  symbol: string;
  algorithmId: string;
  algorithmVersion?: string;
  params?: Record<string, string | number>;
  details?: {
    growthPct: string | null;
    volumeLongRatio: string | null;
    volumeShortRatio: string | null;
    overLong: boolean;
    overShort: boolean;
    missingMinutes: number;
  };
  openTimeUnix: number;
  closeTimeUnix: number;
  closePrice: string;
}

export const localTimezoneLabel = (): string => effectiveTimezone();
export const localTime = (unix: number): string => formatUnix(unix);

export function localDateInput(unix: number): string {
  const date = new Date(unix * 1000);
  if (!Number.isFinite(date.getTime())) return "";
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

/** When the picked date changes, reset the time to 00:00 UTC of that date, shown in the user's local time. */
export function utcMidnightOnDateChange(previous: string, next: string): string {
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(next) || previous.slice(0, 10) === next.slice(0, 10)) return next;
  const [year, month, day] = next.slice(0, 10).split("-").map(Number);
  if (year === undefined || month === undefined || day === undefined) return next;
  const midnight = new Date(Date.UTC(year, month - 1, day));
  const pad = (part: number) => String(part).padStart(2, "0");
  return `${midnight.getFullYear()}-${pad(midnight.getMonth() + 1)}-${pad(midnight.getDate())}T${pad(midnight.getHours())}:${pad(midnight.getMinutes())}`;
}

export function replayRange(from: string, to: string): { fromUnix: number; toUnix: number } {
  const parse = (value: string): number => {
    if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(value)) return NaN;
    const date = new Date(value);
    const parts = [date.getFullYear(), date.getMonth() + 1, date.getDate(), date.getHours(), date.getMinutes()];
    const supplied = value.split(/[-T:]/).map(Number);
    return parts.every((part, index) => part === supplied[index]) ? date.getTime() / 1000 : NaN;
  };
  const fromUnix = parse(from);
  const toUnix = parse(to);
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
