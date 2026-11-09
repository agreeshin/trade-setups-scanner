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

/** Applies (and sanitizes) persisted settings; mirrors them to localStorage when available. */
export function applyInterfaceSettings(raw: unknown): InterfaceSettings {
  current = sanitize(raw);
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(current));
  } catch {
    // Storage may be unavailable; in-memory formatting still applies.
  }
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

function assembleDate(parts: DateParts): string {
  const year = current.yearDigits === 2 ? parts.year.slice(-2) : parts.year;
  const separator = current.dateSeparator;
  if (current.dateOrder === "mdy") return [parts.month, parts.day, year].join(separator);
  if (current.dateOrder === "ymd") return [year, parts.month, parts.day].join(separator);
  return [parts.day, parts.month, year].join(separator);
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
