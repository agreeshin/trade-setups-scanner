import { afterEach, describe, expect, it, vi } from "vitest";
import { applyInterfaceSettings, DEFAULT_INTERFACE_SETTINGS } from "../lib/datetime-format";
import {
  endOfRangeDateChange, localDateInput, localTime, localTimezoneLabel, nowOnTodayDateChange,
  replayRange, utcMidnightOnDateChange, writePlatform,
} from "./platform-controls";

const unix = (iso: string): number => Date.parse(iso) / 1000;

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  applyInterfaceSettings(DEFAULT_INTERFACE_SETTINGS);
});

describe("configured-zone replay time", () => {
  it.each([
    ["UTC", "2026-10-10T12:00:00Z", "2026-10-10T13:00:00Z"],
    ["Asia/Yekaterinburg", "2026-10-10T07:00:00Z", "2026-10-10T08:00:00Z"],
    ["America/New_York", "2026-10-10T16:00:00Z", "2026-10-10T17:00:00Z"],
  ])("converts configured %s wall time to UTC seconds", (timezone, from, to) => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone });
    expect(replayRange("2026-10-10T12:00", "2026-10-10T13:00")).toEqual({
      fromUnix: unix(from), toUnix: unix(to),
    });
  });

  it("keeps the legacy input/time exports aligned with interface settings", () => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "Asia/Yekaterinburg" });
    const instant = unix("2026-01-09T22:05:07Z");
    expect(localDateInput(instant)).toBe("2026-01-10T03:05");
    expect(localTime(instant)).toBe("10.01.2026, 03:05:07");
    expect(localTimezoneLabel()).toBe("Asia/Yekaterinburg");
    expect(localDateInput(NaN)).toBe("");
  });

  it.each([
    ["", "2026-10-10T13:00"],
    ["2026-02-30T12:00", "2026-03-10T13:00"],
    ["2026-10-10T13:00", "2026-10-10T12:00"],
    ["2026-10-10T12:00", "2026-10-10T12:00"],
    ["not-a-date", "2026-10-10T13:00"],
  ])("rejects invalid or reversed range %s to %s", (from, to) => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "UTC" });
    expect(() => replayRange(from, to)).toThrow("корректные даты");
  });

  it("rejects a nonexistent DST time rather than normalizing it into another hour", () => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "America/New_York" });
    expect(() => replayRange("2026-03-08T02:30", "2026-03-08T04:00")).toThrow("корректные даты");
  });

  it("uses the actual 23-hour duration of a spring-forward civil day", () => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "America/New_York" });
    const range = replayRange("2026-03-08T00:00", "2026-03-09T00:00");
    expect(range.toUnix - range.fromUnix).toBe(23 * 3600);
  });
});

describe("picked date defaults", () => {
  it.each([
    ["UTC", "2026-01-10T00:00:00Z"],
    ["Asia/Yekaterinburg", "2026-01-09T19:00:00Z"],
    ["America/New_York", "2026-01-10T05:00:00Z"],
  ])("resets the picked date to configured-zone midnight in %s", (timezone, midnight) => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone });
    const next = utcMidnightOnDateChange("2026-01-09T08:30", "2026-01-10T15:45");
    expect(next).toBe("2026-01-10T00:00");
    expect(replayRange(next, "2026-01-10T00:01").fromUnix).toBe(unix(midnight));
  });

  it("uses the first valid time of the picked day when DST skips midnight", () => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "America/Santiago" });
    expect(utcMidnightOnDateChange("2026-09-05T08:30", "2026-09-06T15:45")).toBe("2026-09-06T01:00");
  });

  it.each([
    ["Asia/Yekaterinburg", "2026-01-09T22:35:49Z", "2026-01-10T03:35", "2026-01-09T09:20"],
    ["America/New_York", "2026-01-10T02:35:49Z", "2026-01-09T21:35", "2026-01-10T09:20"],
  ])("uses configured %s today, not the browser or UTC date", (timezone, now, today, other) => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone });
    vi.useFakeTimers();
    vi.setSystemTime(new Date(now));
    const pickedToday = `${today.slice(0, 10)}T09:20`;
    expect(endOfRangeDateChange(other, pickedToday)).toBe(today);
    expect(nowOnTodayDateChange(other, pickedToday)).toBe(today);
    expect(endOfRangeDateChange(today, other)).toBe(`${other.slice(0, 10)}T00:00`);
    expect(nowOnTodayDateChange(today, other)).toBe(other);
  });

  it("does not replace manually edited times when the date has not changed", () => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "Asia/Yekaterinburg" });
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-01-09T22:35:49Z"));
    for (const change of [utcMidnightOnDateChange, nowOnTodayDateChange, endOfRangeDateChange]) {
      expect(change("2026-01-10T08:00", "2026-01-10T12:45")).toBe("2026-01-10T12:45");
      expect(change("2026-01-10T08:00", "")).toBe("");
      expect(change("2026-01-10T08:00", "2026-02-30T12:45")).toBe("2026-02-30T12:45");
    }
  });

  it("preserves the optimizer's non-today time when its date changes", () => {
    applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "Asia/Yekaterinburg" });
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-01-09T22:35:49Z"));
    expect(nowOnTodayDateChange("2026-01-08T08:00", "2026-01-09T12:45")).toBe("2026-01-09T12:45");
  });
});

describe("platform mutation errors", () => {
  it("shows actionable resource admission details", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify({
      error: "resource_limit_exceeded:work_minutes:120:60",
      resource: "work_minutes",
      requested: 120,
      limit: 60,
    }), { status: 429, headers: { "content-type": "application/json" } })));

    await expect(writePlatform("/v1/replay-runs", {}, "POST"))
      .rejects.toThrow("объём обработки (120 > 60)");
  });
});
