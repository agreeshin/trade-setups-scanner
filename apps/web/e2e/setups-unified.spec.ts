import { expect, test } from "@playwright/test";
import { readFile, writeFile } from "node:fs/promises";

const boot = Date.parse("2026-11-21T12:34:56Z") / 1000;
const recommendation = {
  direction: "long", tpPct: 10, slPct: 5, rawR: 2, meanR: 0.5, robustScore: 0.3,
  timeShiftMinutes: 0, priceChangePct: null, externalName: "Frozen <literal> algorithm", studyId: 40, generationId: 36, slotId: 1, ownerId: "admin",
  clusterEnabled: false, clusterGapMinutes: null, params: { windowMinutes: 5 },
  provenance: { algorithmId: "price_volume_spike", algorithmVersion: "1.1.1", optimizerVersion: "fixture", seed: 1,
    snapshotHash: "browser-fixture", capturedAt: "2026-11-20T00:00:00Z", fromUnix: boot - 86400, toUnix: boot - 3600, observationCutoff: boot - 3600 },
};
const outcome = (riskReturn: number) => ({ status: riskReturn > 0 ? "tp" : "sl", entryTimeUnix: boot - 3600, entryPrice: 100, gateThreshold: null,
  tpPrice: 110, slPrice: 95, closeTimeUnix: boot - 1800, closePrice: riskReturn > 0 ? 110 : 95, riskReturn,
  evaluatedAtUnix: boot, observedThroughUnix: boot - 60, isFinal: true });
const common = { exchange: "bybit", category: "linear", algorithmId: "price_volume_spike", algorithmVersion: "1.1.1", closePrice: "100",
  openTimeUnix: boot - 3660, closeTimeUnix: boot - 3600, params: { windowMinutes: 5 }, details: null };
const rows = [
  { ...common, id: 3, symbol: "SAVED_WATCH", recommendation: null, tradeOutcome: null },
  { ...common, id: 2, symbol: "BTC_LONG", recommendation, tradeOutcome: outcome(2) },
  { ...common, id: 1, symbol: "BTC_SHORT", recommendation: { ...recommendation, direction: "short" }, tradeOutcome: { ...outcome(-1), tpPrice: 90, slPrice: 105, closePrice: 105 } },
];
const canonical = "🟢 BTC_LONG · LONG · Bybit\nЦена: 100.00 USDT\n\nУсловия сделки:\n🎯 TP = 10%, SL = 5% (от фактической цены входа)\n\nАлгоритм: Frozen <literal> algorithm\nИсследование №40 от 21.11.2026";
const csv = "exchange,category,symbol,algorithmId,algorithmVersion,openTimeUnix,closeTimeUnix,closePrice\r\n"
  + rows.map((row) => `${row.exchange},${row.category},${row.symbol},${row.algorithmId},${row.algorithmVersion},${row.openTimeUnix},${row.closeTimeUnix},${row.closePrice}\r\n`).join("");

for (const viewport of [{ width: 2560, height: 1080 }, { width: 375, height: 812 }]) {
  test(`unified Setups isolated acceptance ${viewport.width}px`, async ({ page, baseURL }) => {
    const api: { path: string; method: string; url: string }[] = [];
    const blocked: string[] = [];
    const pageErrors: string[] = [];
    page.on("pageerror", (error) => pageErrors.push(error.message));
    await page.setViewportSize(viewport);
    await page.clock.setFixedTime(new Date(boot * 1000));
    await page.addInitScript(() => {
      window.localStorage.setItem("setups-analytics-filter-v1", JSON.stringify({ algorithmId: "price_volume_spike", from: "2026-11-20T00:00", to: "2026-11-20T12:00", studies: null }));
    });
    // The browser has no production credentials. Every API is a local fixture;
    // any unexpected origin or mutation is blocked before network transmission.
    await page.route("**/*", async (route) => {
      const request = route.request(); const url = new URL(request.url());
      if (url.origin !== new URL(baseURL!).origin) { blocked.push(request.url()); return route.abort(); }
      if (!url.pathname.startsWith("/v1/")) return route.continue();
      const path = url.pathname; const method = request.method();
      api.push({ path, method, url: request.url() });
      if (method !== "GET" && path !== "/v1/setups/outcomes/refresh" && path !== "/v1/setups/analytics") {
        blocked.push(`${method} ${path}`); return route.fulfill({ status: 405, json: { error: "fixture_mutation_blocked" } });
      }
      if (path === "/v1/session") return route.fulfill({ json: { expiresAt: (boot + 3600) * 1000 } });
      if (path === "/v1/status") return route.fulfill({ json: { platform: "ready", marketData: "ready" } });
      if (path === "/v1/settings/ui") return route.fulfill({ json: { value: { setupFilter: "", interface: { timezone: "UTC" } } } });
      if (path === "/v1/algorithms") return route.fulfill({ json: { algorithms: [{ id: "price_volume_spike", name: "Fixture algorithm", version: "1.1.1", description: "", enabled: true, params: {}, fields: [] }] } });
      if (path === "/v1/setups/outcomes/refresh") return route.fulfill({ json: { snapshotId: 3, observedThroughUnix: boot - 60, processed: 2, computed: 0, updated: 0, nextCursor: null } });
      if (path === "/v1/setups/analytics/studies") return route.fulfill({ json: { studies: [{ studyId: 40, externalName: recommendation.externalName, setupCount: 2, generationId: 36, artifactId: null }] } });
      if (path === "/v1/setups/analytics") return route.fulfill({ json: { totals: { selected: 2, gatePassed: 2, closed: 2, meanR: 0.5 }, studies: [],
        setups: rows.filter((row) => row.recommendation !== null).map((row) => ({ id: row.id, symbol: row.symbol, exchange: row.exchange, slotId: 1, studyId: 40,
          direction: row.recommendation.direction, setupTimeUnix: row.closeTimeUnix, ...row.tradeOutcome })) } });
      if (path === "/v1/setups") {
        const q = (url.searchParams.get("q") ?? "").toLowerCase();
        const selected = rows.filter((row) => `${row.symbol} ${row.exchange} ${row.algorithmId}`.toLowerCase().includes(q));
        return route.fulfill({ json: { setups: selected, total: selected.length, offset: 0, limit: 25, snapshotId: 3 } });
      }
      if (path === "/v1/setups/2/details") return route.fulfill({ json: { setup: rows[1], telegramMessage: { text: canonical,
        entities: [{ type: "code", offset: 3, length: 8 }, { type: "bold", offset: canonical.indexOf("Алгоритм"), length: 8 }] }, reason: null } });
      if (path === "/v1/setups/3/details") return route.fulfill({ json: { setup: rows[0], telegramMessage: null, reason: "legacy_watch" } });
      if (path === "/v1/export/setups.csv") return route.fulfill({ body: csv, contentType: "text/csv; charset=utf-8", headers: { "content-disposition": 'attachment; filename="live-setups.csv"' } });
      return route.fulfill({ status: 503, json: { error: "isolated_fixture_unavailable" } });
    });

    await page.goto("/setups");
    await expect(page.getByTestId("setups-analytics-mean")).toHaveText("+0.50R");
    const current = page.locator('[data-page-path="/setups"]');
    await expect(current.locator('[data-slot="page-sidebar"]')).toHaveCount(0);
    await expect(current.locator("h1")).toHaveCount(0);
    await expect(current.getByRole("heading", { name: "Аналитика выпущенных сетапов", exact: true })).toHaveCount(0);
    const geometry = await page.locator('[data-slot="app-main"]').evaluate((main) => {
      const card = main.querySelector('[data-slot="page-card"]')!;
      return { viewport: window.innerWidth, mainWidth: main.getBoundingClientRect().width, cardWidth: card.getBoundingClientRect().width,
        documentWidth: document.documentElement.scrollWidth, maxWidth: getComputedStyle(main).maxWidth };
    });
    expect(geometry.cardWidth).toBeLessThanOrEqual(1920);
    expect(geometry.cardWidth).toBeLessThanOrEqual(viewport.width);
    expect(geometry.documentWidth).toBeLessThanOrEqual(viewport.width);
    if (viewport.width > 1920) expect(geometry.cardWidth).toBe(1920);

    const history = current.getByRole("region", { name: "История сохранённых сетапов" });
    await expect(history.getByRole("cell", { name: "WATCH", exact: true })).toBeVisible();
    await expect(history.getByRole("cell", { name: "LONG", exact: true })).toBeVisible();
    await expect(history.getByRole("cell", { name: "SHORT", exact: true })).toBeVisible();
    await expect(history.getByRole("row").filter({ hasText: "SAVED_WATCH" })).toContainText("Неприменимо");
    await expect(history.getByRole("row").filter({ hasText: "SAVED_WATCH" })).not.toContainText("0.00R");

    await history.getByRole("button", { name: "Подробности SAVED_WATCH", exact: true }).click();
    const watch = page.getByRole("dialog");
    await expect(watch).toContainText("Текст Telegram недоступен для сохранённой записи (legacy_watch).");
    await expect(watch.getByRole("heading", { name: "Сохранённые подробности" })).toBeVisible();
    await expect(watch.locator("pre")).toHaveCount(0);
    await watch.getByRole("button", { name: "Закрыть", exact: true }).click();

    const details = history.getByRole("button", { name: "Подробности BTC_LONG", exact: true });
    await details.click();
    const dialog = page.getByRole("dialog");
    await expect(dialog.locator("pre")).toHaveText(canonical);
    expect(await dialog.locator("pre").textContent()).toBe(canonical);
    await expect(dialog.locator("pre code")).toHaveText("BTC_LONG");
    await expect(dialog.locator("pre strong")).toHaveText("Алгоритм");
    await expect(dialog.getByRole("heading", { name: "Сохранённые подробности" })).toBeVisible();
    await expect(dialog.locator("pre literal")).toHaveCount(0);
    await dialog.getByRole("button", { name: "Закрыть", exact: true }).click();
    await expect(details).toBeFocused();

    await current.getByLabel("Инструмент, биржа или алгоритм", { exact: true }).fill("BTC_SHORT");
    await expect(history.getByRole("cell", { name: "SAVED_WATCH", exact: true })).toHaveCount(0);
    await expect(history.getByRole("cell", { name: "BTC_SHORT", exact: true })).toBeVisible();
    const downloadPromise = page.waitForEvent("download");
    const exportLink = current.getByRole("link", { name: "Экспорт CSV (без фильтра)", exact: true });
    await expect(exportLink).toHaveAttribute("href", "/v1/export/setups.csv");
    await exportLink.click();
    const download = await downloadPromise;
    expect(await download.failure()).toBeNull(); expect(download.suggestedFilename()).toBe("live-setups.csv");
    expect(await readFile(await download.path(), "utf8")).toBe(csv);
    const exportRequest = api.find((request) => request.path === "/v1/export/setups.csv")!;
    expect(new URL(exportRequest.url).search).toBe("");

    await page.getByRole("navigation", { name: "Основная навигация" }).getByRole("link", { name: "Обзор", exact: true }).click();
    await expect(current).toHaveAttribute("hidden", "");
    await page.getByRole("navigation", { name: "Основная навигация" }).getByRole("link", { name: "Сетапы", exact: true }).click();
    await expect(current.getByLabel("Инструмент, биржа или алгоритм", { exact: true })).toHaveValue("BTC_SHORT");
    // A normal former follow-live interval must not create a second mutation.
    await page.waitForTimeout(3200);
    const refreshes = api.filter((request) => request.method === "POST" && request.path === "/v1/setups/outcomes/refresh");
    const calculations = api.filter((request) => request.method === "POST" && request.path === "/v1/setups/analytics");
    const studies = api.filter((request) => request.path === "/v1/setups/analytics/studies");
    expect(refreshes).toHaveLength(1); expect(calculations).toHaveLength(1);
    expect(api.indexOf(refreshes[0]!)).toBeLessThan(api.indexOf(studies[0]!));
    expect(api.indexOf(studies[0]!)).toBeLessThan(api.indexOf(calculations[0]!));
    expect(new URL(calculations[0]!.url).searchParams.get("toUnix")).toBe(String(boot));
    expect(api.filter((request) => request.method !== "GET")).toHaveLength(2);
    expect(blocked).toEqual([]); expect(pageErrors).toEqual([]);
    await page.screenshot({ path: test.info().outputPath(`setups-${viewport.width}.png`), fullPage: true });
    const evidence = test.info().outputPath("unified-setups-dom-evidence.json");
    await writeFile(evidence, JSON.stringify({ geometry, refreshes: refreshes.length, calculations: calculations.length, exportUrl: exportRequest.url, pageErrors, blocked, api }, null, 2));
    await test.info().attach("unified-setups-dom-evidence", { contentType: "application/json", path: evidence });
  });
}
