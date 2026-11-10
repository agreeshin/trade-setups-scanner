import { expect, test } from "@playwright/test";

for (const width of [1440, 375]) {
  test(`replay quality is lazy, paginated and distinct from lifecycle at ${width}`, async ({ page }) => {
    await page.setViewportSize({ width, height: 900 });
    const requests: string[] = [];
    const quality = { version: 1, state: "in_progress", requiredHistoryMinutes: 43200, requestedMinutes: 100, evaluatedMinutes: 20, evaluatedWithGapsMinutes: 2, skippedMinutes: 10, unassessedMinutes: 70, assessedThroughUnix: 1800, reasonCounts: { missing_candle: 0, insufficient_warmup: 0, recent_gap: 10, price_reference_missing: 0, gap_tolerance_exceeded: 0 } };
    const run = { id: 7, name: "Рост цены и объёма", symbol: "BTCUSDT", status: "cancelled", cacheHit: false, setupCount: 0, algorithmId: "price_volume_spike", algorithmVersion: "1", fromUnix: 0, toUnix: 6000, params: {}, quality, qualityComputedAt: null, qualitySourceRunId: 7 };
    await page.route("**/v1/**", async (route) => {
      const url = new URL(route.request().url());
      requests.push(url.pathname + url.search);
      if (url.pathname.endsWith("/quality-intervals")) {
        const offset = Number(url.searchParams.get("offset"));
        return route.fulfill({ json: { available: true, offset, limit: 50, total: 51, intervals: [{ fromUnix: offset * 60, toUnix: (offset + 1) * 60, reason: offset === 0 ? "evaluated" : "recent_gap" }] } });
      }
      if (url.pathname === "/v1/replay-runs/7") return route.fulfill({ json: { run, setups: [], total: 0, offset: 0, limit: 100 } });
      if (url.pathname === "/v1/replay-runs") return route.fulfill({ json: { runs: [run] } });
      if (url.pathname === "/v1/session") return route.fulfill({ json: { authenticated: true, expiresAt: Date.now() + 3600000 } });
      if (url.pathname === "/v1/settings/ui") return route.fulfill({ json: { value: {} } });
      if (url.pathname === "/v1/algorithms") return route.fulfill({ json: { algorithms: [] } });
      if (url.pathname === "/v1/instruments") return route.fulfill({ json: { instruments: [] } });
      return route.fulfill({ json: { platform: "ready", marketData: "ready" } });
    });
    await page.goto("/runs");
    await page.getByRole("button", { name: "Результаты запуска 7" }).click();
    await expect(page.getByRole("heading", { name: /Качество расчёта: Оценена только обработанная часть/ })).toBeVisible();
    expect(requests.some((path) => path.includes("quality-intervals"))).toBe(false);
    await expect(page.getByText(/На 20 пригодных оценённых минутах/)).toBeVisible();
    await page.getByRole("button", { name: "Показать интервалы качества" }).click();
    await page.getByRole("button", { name: "Следующие интервалы" }).click();
    await expect(page.getByText("Интервалов: 51. Страница 2 из 2")).toBeVisible();
    await page.getByRole("button", { name: "Обновить", exact: true }).click();
    await expect(page.getByText("Интервалов: 51. Страница 2 из 2")).toBeVisible();
    expect(requests).toContain("/v1/replay-runs/7/quality-intervals?offset=50&limit=50");
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  });
}
