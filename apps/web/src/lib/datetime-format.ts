/** Interface date/time formatting: order, separator, year digits, 12/24h clock and timezone. */

export type DateOrder = "dmy" | "mdy" | "ymd";
export type DateSeparator = "." | "/" | "-";

export interface InterfaceSettings {
  dateOrder: DateOrder;
  dateSeparator: DateSeparator;
  yearDigits: 4 | 2;
  time24: boolean;
  /** IANA zone name, or "auto" for the browser timezone. */
  timezone: string;
}

export const DEFAULT_INTERFACE_SETTINGS: InterfaceSettings = {
  dateOrder: "dmy",
  dateSeparator: ".",
  yearDigits: 4,
  time24: true,
  timezone: "auto",
};

const STORAGE_KEY = "interface-datetime-settings-v1";
const SETTINGS_CHANGED = "interface-datetime-settings-change";

function sanitize(raw: unknown): InterfaceSettings {
  const value = raw === null || typeof raw !== "object" ? {} : raw as Partial<InterfaceSettings>;
  return {
    dateOrder: value.dateOrder === "mdy" || value.dateOrder === "ymd" ? value.dateOrder : "dmy",
    dateSeparator: value.dateSeparator === "/" || value.dateSeparator === "-" ? value.dateSeparator : ".",
    yearDigits: value.yearDigits === 2 ? 2 : 4,
    time24: value.time24 !== false,
    timezone: typeof value.timezone === "string" && value.timezone !== "" && value.timezone !== "auto" ? value.timezone : "auto",
  };
}

function readStored(): InterfaceSettings {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (raw === null) return DEFAULT_INTERFACE_SETTINGS;
    return sanitize(JSON.parse(raw));
  } catch {
    return DEFAULT_INTERFACE_SETTINGS;
  }
}

let current = readStored();

export function interfaceSettings(): InterfaceSettings {
  return current;
}

/** Mounted date controls unsubscribe on disposal; no retained history/listener registry. */
export function subscribeInterfaceSettings(listener: () => void): () => void {
  window.addEventListener(SETTINGS_CHANGED, listener);
  return () => window.removeEventListener(SETTINGS_CHANGED, listener);
}

/** Applies (and sanitizes) persisted settings; mirrors them to localStorage when available. */
export function applyInterfaceSettings(raw: unknown): InterfaceSettings {
  current = sanitize(raw);
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(current));
  } catch {
    // Storage may be unavailable; in-memory formatting still applies.
  }
  if (typeof window !== "undefined") window.dispatchEvent(new Event(SETTINGS_CHANGED));
  return current;
}

export function effectiveTimezone(): string {
  if (current.timezone !== "auto") return current.timezone;
  return Intl.DateTimeFormat().resolvedOptions().timeZone;
}

function resolvedTimeZone(): string | undefined {
  return current.timezone !== "auto" ? current.timezone : undefined;
}

interface DateParts { year: string; month: string; day: string; hour: string; minute: string; second: string; dayPeriod: string; }

function splitParts(date: Date, withTime: boolean): DateParts {
  const formatter = new Intl.DateTimeFormat("en-US", {
    timeZone: resolvedTimeZone(),
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    ...(withTime ? { hour: "2-digit", minute: "2-digit", second: "2-digit", hourCycle: current.time24 ? "h23" : "h12" } : {}),
  });
  return readParts(date, formatter);
}

function readParts(date: Date, formatter: Intl.DateTimeFormat): DateParts {
  const parts: Record<string, string> = {};
  for (const part of formatter.formatToParts(date)) parts[part.type] = part.value;
  return {
    year: parts.year ?? "",
    month: parts.month ?? "",
    day: parts.day ?? "",
    hour: parts.hour ?? "",
    minute: parts.minute ?? "",
    second: parts.second ?? "",
    dayPeriod: parts.dayPeriod ?? "",
  };
}

function assembleDate(parts: Pick<DateParts, "year" | "month" | "day">): string {
  const year = current.yearDigits === 2 ? parts.year.slice(-2) : parts.year;
  const separator = current.dateSeparator;
  if (current.dateOrder === "mdy") return [parts.month, parts.day, year].join(separator);
  if (current.dateOrder === "ymd") return [year, parts.month, parts.day].join(separator);
  return [parts.day, parts.month, year].join(separator);
}

/** A calendar input is a civil date, not an instant: never shift it through a timezone. */
export function formatCalendarInput(value: string): string {
  const match = /^(\d{4})-(\d{2})-(\d{2})(?:T(\d{2}):(\d{2})(?::(\d{2})(\.\d+)?)?)?$/.exec(value);
  if (!match) return value;
  const date = assembleDate({ year: match[1]!, month: match[2]!, day: match[3]! });
  if (match[4] === undefined) return date;
  const hour = current.time24 ? match[4] : String(Number(match[4]) % 12 || 12).padStart(2, "0");
  const seconds = match[6] === undefined ? "" : `:${match[6]}${match[7] ?? ""}`;
  const period = current.time24 ? "" : Number(match[4]) >= 12 ? " PM" : " AM";
  return `${date}, ${hour}:${match[5]}${seconds}${period}`;
}

export function calendarInputPlaceholder(withTime: boolean): string {
  const year = current.yearDigits === 2 ? "ГГ" : "ГГГГ";
  const parts = current.dateOrder === "dmy" ? ["ДД", "ММ", year] : current.dateOrder === "mdy" ? ["ММ", "ДД", year] : [year, "ММ", "ДД"];
  return `${parts.join(current.dateSeparator)}${withTime ? current.time24 ? ", ЧЧ:ММ" : ", ЧЧ:ММ AM/PM" : ""}`;
}

function assembleTime(parts: DateParts): string {
  const clock = `${parts.hour}:${parts.minute}:${parts.second}`;
  return parts.dayPeriod === "" ? clock : `${clock} ${parts.dayPeriod}`;
}

export function formatUnix(unix: number, options: { time?: boolean } = {}): string {
  const withTime = options.time !== false;
  const parts = splitParts(new Date(unix * 1000), withTime);
  const date = assembleDate(parts);
  return withTime ? `${date}, ${assembleTime(parts)}` : date;
}

export function formatIsoDateTime(iso: string, options: { time?: boolean } = {}): string {
  const date = new Date(iso);
  return Number.isNaN(date.getTime()) ? iso : formatUnix(date.getTime() / 1000, options);
}

/** yyyy-mm-dd key of the calendar day in the effective timezone; grouping key, not for display. */
export function dayKeyOf(unix: number): string {
  const parts = splitParts(new Date(unix * 1000), false);
  return `${parts.year}-${parts.month}-${parts.day}`;
}

/** Each conversion owns one formatter; settings/zone changes retain no formatter cache. */
function wallTimeFormatter(): Intl.DateTimeFormat {
  return new Intl.DateTimeFormat("en-US", {
    timeZone: effectiveTimezone(),
    calendar: "gregory",
    numberingSystem: "latn",
    year: "numeric", month: "2-digit", day: "2-digit",
    hour: "2-digit", minute: "2-digit", second: "2-digit", hourCycle: "h23",
  });
}

/** Encode civil fields on a UTC calendar for arithmetic, not as their actual instant. */
function civilMilliseconds(parts: DateParts): number {
  const date = new Date(0);
  date.setUTCFullYear(Number(parts.year), Number(parts.month) - 1, Number(parts.day));
  date.setUTCHours(Number(parts.hour), Number(parts.minute), Number(parts.second), 0);
  return date.getTime();
}

function wallTimeToUnix(wallMilliseconds: number, formatter: Intl.DateTimeFormat, dayBoundary = false): number {
  let earliest = Infinity;
  let before = -Infinity;
  let after = Infinity;
  // Offsets on either side of an ordinary DST transition supply both candidates.
  // Round-tripping rejects gaps; a repeated wall time deterministically uses the first occurrence.
  for (const hours of [-36, 0, 36]) {
    const sample = wallMilliseconds + hours * 3_600_000;
    const offset = civilMilliseconds(readParts(new Date(sample), formatter)) - sample;
    const candidate = wallMilliseconds - offset;
    const candidateWall = civilMilliseconds(readParts(new Date(candidate), formatter));
    if (candidateWall === wallMilliseconds) {
      earliest = Math.min(earliest, candidate);
    } else if (candidateWall < wallMilliseconds) {
      before = Math.max(before, candidate);
    } else {
      after = Math.min(after, candidate);
    }
  }
  if (Number.isFinite(earliest)) return earliest / 1000;
  if (!dayBoundary || !Number.isFinite(before) || !Number.isFinite(after)) return NaN;
  // Some zones jump over midnight. Calendar boundaries use the first real instant of that day,
  // but explicitly entered nonexistent wall times must still be rejected by the parser.
  while (after - before > 1000) {
    const middle = before + Math.floor((after - before) / 2000) * 1000;
    if (civilMilliseconds(readParts(new Date(middle), formatter)) < wallMilliseconds) before = middle;
    else after = middle;
  }
  return after / 1000;
}

/** Configured-zone wall time for datetime-local controls, independent of display/clock style. */
export function zonedDateTimeInput(unix: number): string {
  const date = new Date(unix * 1000);
  if (!Number.isFinite(date.getTime())) return "";
  const parts = readParts(date, wallTimeFormatter());
  return `${parts.year.padStart(4, "0")}-${parts.month}-${parts.day}T${parts.hour}:${parts.minute}`;
}

/** Wall time in the configured zone -> UTC seconds. Invalid/nonexistent times return NaN. */
export function parseZonedDateTimeInput(value: string): number {
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(value) || Number(value.slice(0, 4)) < 1) return NaN;
  const wall = new Date(`${value}:00.000Z`);
  if (!Number.isFinite(wall.getTime()) || wall.toISOString() !== `${value}:00.000Z`) return NaN;
  return wallTimeToUnix(wall.getTime(), wallTimeFormatter());
}

function zonedDayBoundary(unix: number, days: number): number {
  const date = new Date(unix * 1000);
  if (!Number.isFinite(date.getTime())) return NaN;
  const formatter = wallTimeFormatter();
  const parts = readParts(date, formatter);
  const wall = new Date(civilMilliseconds({ ...parts, hour: "0", minute: "0", second: "0" }));
  wall.setUTCDate(wall.getUTCDate() + days);
  return wallTimeToUnix(wall.getTime(), formatter, true);
}

/** UTC seconds at local midnight (or the day's first instant if a DST gap skips midnight). */
export function zonedDayStart(unix: number): number {
  return zonedDayBoundary(unix, 0);
}

/** Next civil midnight, not a fixed 24-hour step (DST days may have 23 or 25 hours). */
export function nextZonedDay(unix: number): number {
  return zonedDayBoundary(unix, 1);
}

/** Calendar date in the configured timezone, using the configured date layout. */
export function zonedDateLabel(unix: number): string {
  return formatUnix(unix, { time: false });
}
