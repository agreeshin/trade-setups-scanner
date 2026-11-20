import { afterEach, describe, expect, it } from "vitest";
import {
  applyInterfaceSettings, DEFAULT_INTERFACE_SETTINGS, dayKeyOf, effectiveTimezone, formatIsoDateTime,
  formatUnix, interfaceSettings, nextZonedDay, parseZonedDateTimeInput, zonedDateLabel,
  zonedDateTimeInput, zonedDayStart,
} from "./datetime-format";

const T = Date.UTC(2026, 0, 9, 14, 5, 7) / 1000; // 2026-01-09 14:05:07 UTC
const unix = (iso: string): number => Date.parse(iso) / 1000;

afterEach(() => {
  applyInterfaceSettings(DEFAULT_INTERFACE_SETTINGS);
});

describe("datetime format", () => {
  it("formats with the default dd.mm.yyyy 24-hour layout", () => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "UTC" });
    expect(formatUnix(T)).toBe("09.01.2026, 14:05:07");
    expect(formatUnix(T, { time: false })).toBe("09.01.2026");
    expect(effectiveTimezone()).toBe("UTC");
  });

  it("supports order, separator and two-digit year", () => {
    applyInterfaceSettings({ dateOrder: "ymd", dateSeparator: "-", yearDigits: 2, time24: true, timezone: "UTC" });
    expect(formatUnix(T)).toBe("26-01-09, 14:05:07");
    applyInterfaceSettings({ dateOrder: "mdy", dateSeparator: "/", yearDigits: 4, time24: true, timezone: "UTC" });
    expect(formatUnix(T)).toBe("01/09/2026, 14:05:07");
  });

  it("supports the 12-hour clock", () => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "UTC", time24: false });
    expect(formatUnix(T)).toBe("09.01.2026, 02:05:07 PM");
  });

  it("respects an explicit timezone", () => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "Asia/Yekaterinburg" }); // UTC+5
    expect(formatUnix(T)).toBe("09.01.2026, 19:05:07");
    expect(effectiveTimezone()).toBe("Asia/Yekaterinburg");
  });

  it("falls back to defaults for invalid payloads", () => {
    applyInterfaceSettings({ dateOrder: "zzz", dateSeparator: ",", yearDigits: 3, time24: "yes", timezone: 5 });
    expect(interfaceSettings()).toEqual(DEFAULT_INTERFACE_SETTINGS);
  });

  it("formats ISO strings and builds day keys in the effective timezone", () => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "UTC" });
    expect(formatIsoDateTime("2026-01-09T14:05:07.000Z")).toBe("09.01.2026, 14:05:07");
    expect(formatIsoDateTime("not a date")).toBe("not a date");
    expect(dayKeyOf(T)).toBe("2026-01-09");
  });
});

describe("configured-zone calendar foundations", () => {
  it.each([
    ["UTC", "2026-01-09T22:05", "2026-01-09T00:00:00Z", "2026-01-10T00:00:00Z", "09.01.2026"],
    ["Asia/Yekaterinburg", "2026-01-10T03:05", "2026-01-09T19:00:00Z", "2026-01-10T19:00:00Z", "10.01.2026"],
    ["America/New_York", "2026-01-09T17:05", "2026-01-09T05:00:00Z", "2026-01-10T05:00:00Z", "09.01.2026"],
    ["Asia/Kathmandu", "2026-01-10T03:50", "2026-01-09T18:15:00Z", "2026-01-10T18:15:00Z", "10.01.2026"],
  ])("uses %s for input, parsing, boundaries and labels", (timezone, input, start, next, label) => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone });
    const instant = unix("2026-01-09T22:05:07Z");
    expect(zonedDateTimeInput(instant)).toBe(input);
    expect(parseZonedDateTimeInput(input)).toBe(instant - 7);
    expect(zonedDayStart(instant)).toBe(unix(start));
    expect(nextZonedDay(instant)).toBe(unix(next));
    expect(dayKeyOf(instant)).toBe(input.slice(0, 10));
    expect(zonedDateLabel(instant)).toBe(label);
  });

  it("uses a configured zone different from the browser timezone", () => {
    const browser = Intl.DateTimeFormat().resolvedOptions().timeZone;
    const timezone = browser === "Asia/Yekaterinburg" ? "America/New_York" : "Asia/Yekaterinburg";
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone });
    expect(effectiveTimezone()).not.toBe(browser);
    const input = timezone === "Asia/Yekaterinburg" ? "2026-01-10T03:05" : "2026-01-09T17:05";
    expect(zonedDateTimeInput(unix("2026-01-09T22:05:00Z"))).toBe(input);
    expect(parseZonedDateTimeInput(input)).toBe(unix("2026-01-09T22:05:00Z"));
  });

  it("uses the browser timezone in auto mode", () => {
    applyInterfaceSettings(DEFAULT_INTERFACE_SETTINGS);
    expect(effectiveTimezone()).toBe(Intl.DateTimeFormat().resolvedOptions().timeZone);
    const date = new Date(2026, 0, 9, 14, 5);
    const instant = date.getTime() / 1000;
    expect(zonedDateTimeInput(instant)).toBe("2026-01-09T14:05");
    expect(parseZonedDateTimeInput("2026-01-09T14:05")).toBe(instant);
    expect(zonedDayStart(instant)).toBe(new Date(2026, 0, 9).getTime() / 1000);
    expect(nextZonedDay(instant)).toBe(new Date(2026, 0, 10).getTime() / 1000);
  });

  it("keeps machine inputs in ISO/24-hour form while applying the display date layout", () => {
    applyInterfaceSettings({ dateOrder: "mdy", dateSeparator: "/", yearDigits: 2, time24: false, timezone: "Asia/Yekaterinburg" });
    const instant = unix("2026-01-09T22:05:00Z");
    expect(zonedDateTimeInput(instant)).toBe("2026-01-10T03:05");
    expect(parseZonedDateTimeInput("2026-01-10T03:05")).toBe(instant);
    expect(zonedDateLabel(instant)).toBe("01/10/26");
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "America/New_York" });
    expect(zonedDateTimeInput(instant)).toBe("2026-01-09T17:05");
  });

  it.each([
    "", "2026-01-09", "2026-1-09T12:00", "2026-01-09T12:00Z", "2026-01-09T12:00:01",
    "2026-01-09T12:00+05:00", "2026-00-09T12:00", "2026-13-09T12:00", "2026-01-00T12:00",
    "2026-01-32T12:00", "2026-02-29T12:00", "2026-02-30T12:00", "2026-01-09T24:00",
    "2026-01-09T12:60", "0000-01-01T00:00", "not-a-date",
  ])("rejects malformed or invalid civil input %s", (value) => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "UTC" });
    expect(parseZonedDateTimeInput(value)).toBeNaN();
  });

  it("accepts leap dates and does not remap years below 100 to the twentieth century", () => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "UTC" });
    for (const value of ["2024-02-29T00:00", "0099-01-09T14:05"]) {
      const instant = unix(`${value}:00Z`);
      expect(parseZonedDateTimeInput(value)).toBe(instant);
      expect(zonedDateTimeInput(instant)).toBe(value);
    }
  });

  it.each([
    ["UTC", "2024-02-28T12:00:00Z", "2024-02-29T00:00:00Z"],
    ["Asia/Yekaterinburg", "2026-12-31T12:00:00Z", "2026-12-31T19:00:00Z"],
  ])("advances civil dates across month/year boundaries in %s", (timezone, instant, next) => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone });
    expect(nextZonedDay(unix(instant))).toBe(unix(next));
  });

  it("returns empty/invalid results for invalid instants", () => {
    expect(zonedDateTimeInput(NaN)).toBe("");
    expect(zonedDateTimeInput(Infinity)).toBe("");
    expect(zonedDayStart(NaN)).toBeNaN();
    expect(nextZonedDay(Infinity)).toBeNaN();
  });
});

describe("configured-zone DST", () => {
  it("uses the first actual instant of a civil day when DST skips midnight", () => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "America/Santiago" });
    const first = unix("2026-09-06T04:00:00Z");
    expect(zonedDateTimeInput(first)).toBe("2026-09-06T01:00");
    expect(parseZonedDateTimeInput("2026-09-06T00:00")).toBeNaN();
    expect(zonedDayStart(unix("2026-09-06T16:00:00Z"))).toBe(first);
    expect(nextZonedDay(unix("2026-09-05T16:00:00Z"))).toBe(first);
    expect(nextZonedDay(first)).toBe(unix("2026-09-07T03:00:00Z"));
  });

  it.each([
    ["America/New_York", "2026-03-08T16:00:00Z", "2026-03-08T05:00:00Z", "2026-03-09T04:00:00Z", 23],
    ["America/New_York", "2026-11-01T17:00:00Z", "2026-11-01T04:00:00Z", "2026-11-02T05:00:00Z", 25],
    ["Europe/Berlin", "2026-03-29T12:00:00Z", "2026-03-28T23:00:00Z", "2026-03-29T22:00:00Z", 23],
    ["Europe/Berlin", "2026-10-25T12:00:00Z", "2026-10-24T22:00:00Z", "2026-10-25T23:00:00Z", 25],
  ])("uses civil midnight across DST in %s at %s", (timezone, instant, start, next, hours) => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone });
    const startUnix = zonedDayStart(unix(instant));
    expect(startUnix).toBe(unix(start));
    expect(zonedDayStart(startUnix)).toBe(startUnix);
    expect(nextZonedDay(unix(instant))).toBe(unix(next));
    expect(nextZonedDay(startUnix)).toBe(unix(next));
    expect(unix(next) - startUnix).toBe(Number(hours) * 3600);
    expect(zonedDateTimeInput(startUnix).slice(11)).toBe("00:00");
    expect(zonedDateTimeInput(unix(next)).slice(11)).toBe("00:00");
  });

  it.each([
    ["America/New_York", "2026-03-08T02:30"],
    ["Europe/Berlin", "2026-03-29T02:30"],
  ])("rejects nonexistent spring-forward wall time in %s", (timezone, value) => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone });
    expect(parseZonedDateTimeInput(value)).toBeNaN();
  });

  it.each([
    ["America/New_York", "2026-11-01T01:30", "2026-11-01T05:30:00Z"],
    ["Europe/Berlin", "2026-10-25T02:30", "2026-10-25T00:30:00Z"],
  ])("chooses the first occurrence of repeated fall-back wall time in %s", (timezone, value, first) => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone });
    expect(parseZonedDateTimeInput(value)).toBe(unix(first));
    expect(zonedDateTimeInput(unix(first))).toBe(value);
    expect(zonedDateTimeInput(unix(first) + 3600)).toBe(value);
  });
});
