import { afterEach, describe, expect, it } from "vitest";
import { applyInterfaceSettings, DEFAULT_INTERFACE_SETTINGS, dayKeyOf, effectiveTimezone, formatIsoDateTime, formatUnix, interfaceSettings } from "./datetime-format";

const T = Date.UTC(2026, 0, 9, 14, 5, 7) / 1000; // 2026-01-09 14:05:07 UTC

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
