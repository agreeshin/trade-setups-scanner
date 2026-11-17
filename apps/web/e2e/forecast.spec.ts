import { expect, test } from "@playwright/test";
import { forecastFixture } from "../test/forecast-fixture";
import type { ForecastCurveRequest } from "../src/lib/forecast-api";

const cutoff = Date.parse("2026-01-13T00:00:00Z") / 1000;
const day = Date.parse("2026-01-01T00:00:00Z") / 1000;
const recipe = { method: "decayed_probability", version: 1, halfLifeDays: 14, kappa: 10, priorPolicy: "gross_break_even" } as const;

for (const viewport of [{ name: "desktop", width: 1280, height: 960 }, { name: "mobile", width: 375, height: 812 }]) {
  for (const colorScheme of ["dark", "light"] as const) {
    test(`probability graph preserves model identity and readable ${viewport.name} ${colorScheme} geometry`, async ({ page }, testInfo) => {
      await page.setViewportSize({ width: viewport.width, height: viewport.height });
      await page.emulateMedia({ colorScheme });
      await page.addInitScript((now) => { Date.now = () => now; }, cutoff * 1000);
      let graphRequest: ForecastCurveRequest | null = null;
      let matrixCutoff: number | null = null;
      const pageErrors: string[] = [];
      page.on("pageerror", (error) => pageErrors.push(error.message));
      const rows = [0, 2, 4, 7, 9, 10].map((offset, index) => {
        const signal = day + offset * 86400 + 12 * 3600;
        return { setup: { exchange: "bybit", category: "linear", symbol: `DEMO${index}USDT`, openTimeUnix: signal - 60, closeTimeUnix: signal, closePrice: "100" },
          direction: "short", tpPct: 2, slPct: 1, status: index === 0 ? "unclosed" : index % 2 ? "tp" : "sl", entryPrice: 100, tpPrice: 98, slPrice: 101,
          closeTimeUnix: index === 0 ? null : signal + 600, riskReturn: index === 0 ? null : index % 2 ? 2 : -1 };
      });
      await page.route("**/v1/**", async (route) => {
        const request = route.request();
        const url = new URL(request.url());
        const path = url.pathname;
        const json = (value: unknown) => route.fulfill({ json: value });
        if (path === "/v1/session") return json({ authenticated: true, expiresAt: Date.now() + 3600000 });
        if (path === "/v1/status") return json({ platform: "ready", marketData: "ready" });
        if (path === "/v1/settings/ui") return json({ value: {} });
        if (path === "/v1/analytics/cache/summary") return json({ tables: [], total: 0 });
        if (path === "/v1/settings/web-analytics") return json({ settings: { trendMode: "blend", trendBlendLambda: "0.7", forecast: recipe,
          sections: { "short-trades": { tpInput: "2", slInput: "1" } } } });
        if (path === "/v1/analytics/sets") return json({ sets: [{ setId: "run-12", kind: "run", numericId: 12, label: "#12 · Forecast fixture", algorithmId: "price_volume_spike", algorithmVersion: "1.1.1", params: {}, exchange: "bybit", category: "linear", symbol: null,
          setupCount: rows.length, fromUnix: day, toUnix: day + 11 * 86400, createdAt: "2026-01-12T00:00:00Z" }] });
        if (path === "/v1/analytics/months") return json({ months: ["2026-01"] });
        if (path === "/v1/analytics/matrix-grid") {
          matrixCutoff = Number(url.searchParams.get("observationCutoff"));
          return json({ items: [{ tpPct: 4, slPct: 4, avgMovePct: 0.52, riskExpectancy: 0.52, meanR: 0.8, forecastR: 0.4, decisionR: 0.52, forecast: recipe, observationCutoff: matrixCutoff }], totalMatchingSetups: rows.length, avgSelectionOrdinal: null, avgSelectionDelayMinutes: null, totalClustersCount: null, cachedSelection: true });
        }
        if (path === "/v1/analytics/setups") return json({ setups: rows.map((row) => row.setup), totalMatchingSetups: rows.length, avgSelectionOrdinal: null, avgSelectionDelayMinutes: null, totalClustersCount: null, cachedSelection: true });
        if (path === "/v1/analytics/trades") return json({ items: rows, cachedCount: rows.length });
        if (path === "/v1/analytics/forecast") {
          graphRequest = request.postDataJSON() as ForecastCurveRequest;
          const response = forecastFixture(graphRequest);
          response.curve[0]!.forecastR = null;
          response.blendCurve![0]!.decisionR = null;
          return json(response);
        }
        return route.fulfill({ status: 503, json: { error: "fixture unavailable" } });
      });
      await page.goto("/analytics");
      await expect(page.getByLabel("Метод прогноза", { exact: true })).toHaveValue("decayed_probability");
      await page.getByRole("button", { name: "Рассчитать", exact: true }).nth(0).click();
      await expect(page.getByText("0.52", { exact: true })).toBeVisible();
      await page.getByRole("button", { name: "Рассчитать", exact: true }).nth(1).click();
      const chart = page.getByRole("region", { name: "Вероятностный прогноз матожидания" });
      await expect(chart.getByTestId("forecast-model-curve")).toBeVisible();
      await expect(chart.getByLabel("Чистый прогноз модели")).toHaveText("0.4 R");
      await expect(chart.getByLabel("Метрика решения")).toHaveText("0.52 R");
      await expect(chart.getByText(/Ретроспективная модельная оценка, не causal\/OOS/)).toBeVisible();
      expect(graphRequest).not.toBeNull();
      expect((graphRequest as ForecastCurveRequest | null)?.observationCutoff).toBe(cutoff);
      expect(matrixCutoff).toBe(cutoff);
      const flat = chart.getByTestId("forecast-flat-future");
      expect(await flat.getAttribute("y1")).toBe(await flat.getAttribute("y2"));
      expect(await chart.getByTestId("forecast-model-curve").getAttribute("points")).not.toContain("NaN");
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
      expect(pageErrors).toEqual([]);
      // Exclude only the sticky shell overlay from graph-only evidence; the full-page capture is unmodified.
      await chart.screenshot({ path: testInfo.outputPath(`forecast-${viewport.name}-${colorScheme}.png`), style: "header { visibility: hidden !important; }" });
      if (viewport.name === "mobile") {
        const plot = chart.getByTestId("forecast-plot-viewport");
        expect(await plot.evaluate((element) => element.scrollWidth > element.clientWidth)).toBe(true);
        await plot.evaluate((element) => { element.scrollLeft = element.scrollWidth; });
        expect(await plot.evaluate((element) => element.scrollLeft)).toBeGreaterThan(0);
        await chart.screenshot({ path: testInfo.outputPath(`forecast-${viewport.name}-${colorScheme}-cutoff.png`), style: "header { visibility: hidden !important; }" });
      }
      await page.screenshot({ path: testInfo.outputPath(`forecast-page-${viewport.name}-${colorScheme}.png`), fullPage: true });
    });
  }
}
