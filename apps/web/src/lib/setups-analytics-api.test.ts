import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { SetupsAnalyticsApiError, fetchExpectancyPngUrl, fetchSetupsAnalytics, fetchSetupsAnalyticsStudies } from "./setups-analytics-api";

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });
const studyMeta = { studyId: 40, externalName: "Всплеск цены и оборота", setupCount: 12, generationId: 36, artifactId: 900 };
const setup = {
  id: 9, symbol: "BTCUSDT", exchange: "bybit", slotId: 1, studyId: 40, direction: "short",
  setupTimeUnix: 100, entryTimeUnix: 400, entryPrice: 98.5, gateThreshold: 98, tpPrice: 78.8,
  slPrice: 104.41, closeTimeUnix: 900, riskReturn: -1, status: "sl",
};
const result = {
  totals: { selected: 5, gatePassed: 3, closed: 2, meanR: 1.25 },
  studies: [{ studyId: 40, selected: 5, gatePassed: 3, closed: 2, meanR: 1.25 }],
  setups: [setup],
};

const calls: string[] = [];
beforeEach(() => {
  calls.length = 0;
  Object.defineProperty(URL, "createObjectURL", { configurable: true, writable: true, value: vi.fn(() => "blob:expectancy-png") });
  Object.defineProperty(URL, "revokeObjectURL", { configurable: true, writable: true, value: vi.fn() });
  vi.stubGlobal("fetch", vi.fn((path: string) => {
    calls.push(path);
    return Promise.resolve(json({ studies: [studyMeta] }));
  }));
});
afterEach(() => { vi.unstubAllGlobals(); vi.restoreAllMocks(); });

it("loads releaser studies for the algorithm and range", async () => {
  const items = await fetchSetupsAnalyticsStudies("price_volume_spike", 1, 2);
  expect(items).toHaveLength(1);
  expect(items[0]!.studyId).toBe(40);
  expect(items[0]!.artifactId).toBe(900);
  expect(calls[0]).toContain("/v1/setups/analytics/studies?algorithmId=price_volume_spike&fromUnix=1&toUnix=2");
});

it("passes selected studies and parses the full result", async () => {
  vi.stubGlobal("fetch", vi.fn((path: string) => {
    calls.push(path);
    return Promise.resolve(json(result));
  }));
  const data = await fetchSetupsAnalytics("a", 1, 2, [40, 41]);
  expect(data.totals.meanR).toBe(1.25);
  expect(data.studies[0]!.studyId).toBe(40);
  expect(data.setups[0]!.status).toBe("sl");
  expect(new URL(calls[0]!, "http://localhost").searchParams.get("studies")).toBe("40,41");
});

it("omits studies when all are selected", async () => {
  vi.stubGlobal("fetch", vi.fn((path: string) => {
    calls.push(path);
    return Promise.resolve(json(result));
  }));
  await fetchSetupsAnalytics("a", 1, 2, null);
  expect(new URL(calls[0]!, "http://localhost").searchParams.has("studies")).toBe(false);
});

it("loads the artifact png as an object url", async () => {
  vi.stubGlobal("fetch", vi.fn((path: string) => {
    calls.push(path);
    return Promise.resolve(new Response(new Blob(["png-bytes"]), { status: 200 }));
  }));
  const url = await fetchExpectancyPngUrl(900);
  expect(url).toBe("blob:expectancy-png");
  expect(calls[0]).toContain("/v1/setups/analytics/expectancy-png/900");
});

it("reports png http failures", async () => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ error: "expectancy_png_unavailable" }, 404))));
  await expect(fetchExpectancyPngUrl(900)).rejects.toThrow("HTTP 404");
});

it("maps the limit error to a range hint", async () => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ error: "setups_analytics_limit_exceeded" }, 400))));
  const cause = await fetchSetupsAnalytics("a", 1, 2, null).catch((error: unknown) => error);
  expect(cause).toBeInstanceOf(SetupsAnalyticsApiError);
  expect((cause as SetupsAnalyticsApiError).message).toContain("Сузьте диапазон");
});

it("reports other HTTP failures with the status", async () => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ error: "boom" }, 500))));
  await expect(fetchSetupsAnalytics("a", 1, 2, null)).rejects.toThrow("HTTP 500");
  await expect(fetchSetupsAnalyticsStudies("a", 1, 2)).rejects.toThrow("HTTP 500");
});

it("rejects malformed payloads", async () => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ totals: { selected: -1 } }))));
  await expect(fetchSetupsAnalytics("a", 1, 2, null)).rejects.toThrow(SetupsAnalyticsApiError);
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ studies: [{ studyId: "x" }] }))));
  await expect(fetchSetupsAnalyticsStudies("a", 1, 2)).rejects.toThrow(/исследований|studies/);
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ setups: [{ id: 1, status: "unknown" }] }))));
  await expect(fetchSetupsAnalytics("a", 1, 2, null)).rejects.toThrow(SetupsAnalyticsApiError);
});
