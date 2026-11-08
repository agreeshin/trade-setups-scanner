import { expect, test } from "@playwright/test";

test("mobile preflight requires explicit snapshot confirmation and invalidates on edit", async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 900 });
  const writes: { path: string; body: unknown }[] = [];
  const snapshot = { instruments: [{ exchange: "bybit", category: "linear", symbol: "BTCUSDT" }], fromUnix: 100, toUnix: 200, algorithmId: "price_volume_spike", algorithmVersion: "snapshot-v2", params: { priceGrowthPct: "18", baselineLongDays: 30 }, async: true };
  await page.route("**/v1/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (route.request().method() === "POST") writes.push({ path, body: route.request().postDataJSON() as unknown });
    if (path === "/v1/session") return route.fulfill({ json: { authenticated: true, expiresAt: Date.now() + 3600000 } });
    if (path === "/v1/algorithms") return route.fulfill({ json: { algorithms: [{ id: "price_volume_spike", name: "Алгоритм", version: "old-v1" }] } });
    if (path === "/v1/instruments") return route.fulfill({ json: { instruments: [{ ...snapshot.instruments[0], id: 1, status: "active" }] } });
    if (path === "/v1/settings/ui") return route.fulfill({ json: { value: { replay: { symbols: "BTCUSDT", algorithmId: "price_volume_spike", from: "2026-10-01T12:00", to: "2026-10-01T13:00" } } } });
    if (path === "/v1/replay-preflight") return route.fulfill({ json: { checkedAt: "2026-10-01T12:00:00Z", algorithmVersion: "snapshot-v2", requiredHistoryMinutes: 43200, canRun: true, request: snapshot, results: [{ ...snapshot.instruments[0], cacheHit: false, status: "ready", coverage: { instrumentId: 1, fromUnix: 0, toUnix: 200, splitUnix: 100, expectedMinutes: 3, presentMinutes: 0, missingMinutes: 3, warmupExpectedMinutes: 2, warmupPresentMinutes: 0, signalExpectedMinutes: 1, signalPresentMinutes: 0, gaps: [{ fromUnix: 0, toUnix: 200 }], gapCount: 1, offset: 0, limit: 50, checkedAt: "2026-10-01T12:00:00Z" } }] } });
    if (path === "/v1/replay-runs") return route.fulfill({ json: { runs: [] } });
    return route.fulfill({ json: { platform: "ready", marketData: "ready" } });
  });
  await page.goto("/runs");
  await page.getByRole("button", { name: "Проверить данные" }).click();
  await page.getByRole("button", { name: "Показать проверенные наборы свечей" }).click();
  await expect(page.getByText(/Проверено: в этом входном диапазоне истории нет/)).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(page.getByText(/Проверено: в этом входном диапазоне истории нет/)).toHaveCount(0);
  expect(await page.evaluate(() => document.body.hasAttribute("data-scroll-locked"))).toBe(false);
  expect(writes.map((item) => item.path)).toEqual(["/v1/replay-preflight"]);
  await page.getByLabel("Конец", { exact: true }).fill("2026-10-01T14:00");
  await expect(page.getByRole("button", { name: "Запустить проверенный расчёт" })).toHaveCount(0);
  await page.getByRole("button", { name: "Проверить данные" }).click();
  await expect(page.getByRole("button", { name: "Запустить проверенный расчёт" })).toBeEnabled();
  expect(writes.every((item) => item.path === "/v1/replay-preflight")).toBe(true);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.getByRole("button", { name: "Запустить проверенный расчёт" }).click();
  await expect(page.getByText(/Запрос запуска принят/)).toBeVisible();
  expect(writes.at(-1)).toEqual({ path: "/v1/replay-runs", body: snapshot });
  expect(writes).toHaveLength(3);
});
