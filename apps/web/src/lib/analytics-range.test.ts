import { afterEach, expect, it, vi } from "vitest";
import { allAnalyticsRange, monthAnalyticsRange, normalizeAnalyticsTimeRange, requireAnalyticsRange, parseAnalyticsAxis, SHORT_MATRIX_LEVELS, LONG_TP_LEVELS, LONG_SL_LEVELS } from "./analytics-settings";
import { applyInterfaceSettings, parseZonedDateTimeInput, zonedDateTimeInput } from "./datetime-format";
import { fetchAnalyticsSetups, fetchMatrixCell, fetchMatrixCells, fetchMatrixGrid, fetchTradeRow, fetchTradeRows } from "./analytics-api";

afterEach(() => { vi.unstubAllGlobals(); applyInterfaceSettings({ timezone: "auto" }); });
const unix = (iso: string) => Date.parse(iso) / 1000;

it("validates bounded independent TP/SL axes, rejects numeric duplicates and preserves order/defaults", () => {
  expect(parseAnalyticsAxis(" 8, 2.5, .25, 1e-6 ", "TP")).toEqual([8, 2.5, 0.25, 0.000001]);
  expect(() => parseAnalyticsAxis("1,1.0", "TP")).toThrow("повторяющиеся");
  expect(() => parseAnalyticsAxis(".25,2.5e-1", "SL")).toThrow("повторяющиеся");
  expect(SHORT_MATRIX_LEVELS).toEqual(Array.from({ length: 14 }, (_, index) => 4 + index * 2));
  expect(LONG_TP_LEVELS).toHaveLength(22); expect(LONG_SL_LEVELS).toHaveLength(25);
  for (const raw of ["", " ", ",", "4,", "4,,6", "NaN", "Infinity", "-1", "0", "100.1", "0.0000009", "0x10", "1e999", "4;6", Array.from({ length: 65 }, (_, index) => index + 1).join(","), "1".repeat(4097)]) {
    expect(() => parseAnalyticsAxis(raw, "SL"), raw).toThrow("SL:");
  }
  const max = Array.from({ length: 64 }, (_, index) => index + 1).join(",");
  expect(parseAnalyticsAxis(max, "TP").length * parseAnalyticsAxis(max, "SL").length).toBe(4096);
});

it.each(["America/New_York", "Pacific/Auckland", "Asia/Kathmandu"])("migrates legacy UTC days and months without mutating their source in %s", (timezone) => {
  applyInterfaceSettings({ timezone });
  const legacy = { mode: "custom", fromDay: "2026-03-08", toDay: "2026-03-08" };
  const before = JSON.stringify(legacy);
  const days = normalizeAnalyticsTimeRange(legacy);
  expect(requireAnalyticsRange(days)).toEqual({ fromUnix: unix("2026-03-08T00:00:00Z"), toUnix: unix("2026-03-09T00:00:00Z") });
  expect(JSON.stringify(legacy)).toBe(before);
  const month = normalizeAnalyticsTimeRange({ mode: "month", month: "2026-03" });
  expect(requireAnalyticsRange(month)).toEqual({ fromUnix: unix("2026-03-01T00:00:00Z"), toUnix: unix("2026-04-01T00:00:00Z") });
  const serialized = JSON.stringify(days);
  applyInterfaceSettings({ timezone: "Pacific/Honolulu" });
  expect(normalizeAnalyticsTimeRange(JSON.parse(serialized))).toEqual(days);
  expect(zonedDateTimeInput(days.fromUnix!)).toBe("2026-03-07T14:00");
  expect(normalizeAnalyticsTimeRange(month)).toEqual(month);
});

it("new month presets and civil input use the interface zone across DST, unlike migrated UTC months", () => {
  applyInterfaceSettings({ timezone: "America/New_York" });
  const march = monthAnalyticsRange("2026-03");
  expect(requireAnalyticsRange(march)).toEqual({ fromUnix: unix("2026-03-01T05:00:00Z"), toUnix: unix("2026-04-01T04:00:00Z") });
  expect(requireAnalyticsRange(normalizeAnalyticsTimeRange({ mode: "month", month: "2026-03" }))).not.toEqual(requireAnalyticsRange(march));
  expect(parseZonedDateTimeInput("2026-03-08T01:30")).toBe(unix("2026-03-08T06:30:00Z"));
  expect(parseZonedDateTimeInput("2026-03-08T02:30")).toBeNaN();
  expect(parseZonedDateTimeInput("2026-03-08T03:30")).toBe(unix("2026-03-08T07:30:00Z"));
  expect(parseZonedDateTimeInput("2026-11-01T01:30")).toBe(unix("2026-11-01T05:30:00Z")); // First repeated time, same as Runs.
  const all = allAnalyticsRange({ fromUnix: unix("2026-03-08T06:30:00Z"), toUnix: unix("2026-03-08T07:30:00Z") });
  expect(requireAnalyticsRange(all)).toEqual({ fromUnix: unix("2026-03-08T05:00:00Z"), toUnix: unix("2026-03-09T04:00:00Z") });
});

it("normalizes saved integer seconds to minutes while retaining exact invalid/missing ranges for explicit validation", () => {
  const exact = normalizeAnalyticsTimeRange({ mode: "custom", fromUnix: 6001, toUnix: 9659, fromDay: "1999-01-01", toDay: "1999-01-01" });
  expect(requireAnalyticsRange(exact)).toEqual({ fromUnix: 6000, toUnix: 9600 });
  for (const bounds of [{ fromUnix: 6000, toUnix: 6000 }, { fromUnix: 9600, toUnix: 6000 }, { fromUnix: null, toUnix: 9600 }, { fromUnix: 6000.5, toUnix: 9600 }]) {
    expect(() => requireAnalyticsRange(normalizeAnalyticsTimeRange({ mode: "custom", ...bounds }))).toThrow("конец диапазона");
  }
});

it("emits only the exact pair for every calculation route and rejects ambiguous or incomplete API requests before fetch", async () => {
  const fetch = vi.fn(() => Promise.resolve(new Response(JSON.stringify({ items: [] }))));
  vi.stubGlobal("fetch", fetch);
  const params = { set: "run-2", fromUnix: 6000, toUnix: 9600 };
  const setup = { exchange: "bybit", category: "linear", symbol: "BTCUSDT", openTimeUnix: 5940, closeTimeUnix: 6000, closePrice: "100" };
  await fetchAnalyticsSetups(params); await fetchMatrixCell(params); await fetchMatrixCells(params);
  await fetchMatrixGrid(params, [4], [4]); await fetchTradeRows(params); await fetchTradeRow(setup, params);
  expect(fetch).toHaveBeenCalledTimes(6);
  for (const [url] of fetch.mock.calls as unknown as [string][]) {
    const query = new URLSearchParams(url.split("?")[1]);
    expect(query.get("fromUnix")).toBe("6000"); expect(query.get("toUnix")).toBe("9600");
    for (const key of ["month", "fromDay", "toDay"]) expect(query.has(key)).toBe(false);
  }
  for (const extra of [{ month: "2026-03" }, { fromDay: "2026-03-08" }, { toDay: "2026-03-08" }]) {
    await expect(fetchTradeRows({ ...params, ...extra })).rejects.toThrow("смешивать");
  }
  await expect(fetchTradeRows({ set: "run-2", fromUnix: 6000 })).rejects.toThrow("fromUnix < toUnix");
  await expect(fetchTradeRows({ ...params, fromUnix: 6001 })).rejects.toThrow("fromUnix < toUnix");
  await expect(fetchTradeRows({ ...params, toUnix: 9601 })).rejects.toThrow("fromUnix < toUnix");
  expect(fetch).toHaveBeenCalledTimes(6);
});
