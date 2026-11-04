import { expect, test } from "@playwright/test";
test.beforeEach(async ({ page }) => {
  await page.emulateMedia({ colorScheme: "dark" });
  await page.route("**/v1/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path === "/v1/session") return route.fulfill({ json: { authenticated: true, expiresAt: Date.now() + 3600000 } });
    if (path === "/v1/status") return route.fulfill({ json: { platform: "ready", marketData: "not_ready" } });
    if (path === "/v1/instruments") return route.fulfill({ json: { instruments: [{ id: 1, exchange: "bybit", category: "linear", symbol: "BTCUSDT", status: "Trading" }] } });
    if (path === "/v1/algorithms") return route.fulfill({ json: { algorithms: [{ id: "price_volume_spike", name: "Рост цены и объёма", version: "1", description: "Общие настройки", params: {}, fields: [], enabled: true }] } });
    if (path === "/v1/settings/ui") return route.fulfill({ json: { value: {} } });
    if (path === "/v1/setups") return route.fulfill({ json: { setups: [] } });
    if (path === "/v1/replay-runs") return route.fulfill({ json: { runs: [] } });
    if (path === "/v1/history/backfill-jobs") return route.fulfill({ json: { jobs: [] } });
    if (path === "/v1/history/collection-targets") return route.fulfill({ json: { targets: [] } });
    if (path === "/v1/history/instruments/1/coverage") return route.fulfill({ json: { instrumentId: 1, firstMinuteUnix: null, lastMinuteUnix: null, totalCandles: 0, intervals: [], gaps: [] } });
    return route.fulfill({ status: 503, json: { error: "fixture unavailable" } });
  });
});
test.describe("application smoke", () => {
  test("theme preference supports system changes, explicit override and reload", async ({ page }) => {
    await page.emulateMedia({ colorScheme: "light" });
    await page.goto("/");
    const html = page.locator("html");
    const theme = page.getByRole("combobox", { name: "Тема интерфейса" });
    const surface = page.locator('[data-slot="app-surface"]');
    await expect(theme).toHaveValue("system");
    await expect(html).toHaveAttribute("data-theme", "system");
    await expect(surface).toHaveCSS("background-color", "rgb(248, 247, 250)");
    await page.emulateMedia({ colorScheme: "dark" });
    await expect(surface).toHaveCSS("background-color", "rgb(16, 17, 19)");
    await theme.selectOption("light");
    await expect(html).toHaveAttribute("data-theme", "light");
    await expect(surface).toHaveCSS("background-color", "rgb(248, 247, 250)");
    await page.reload();
    await expect(theme).toHaveValue("light");
    await expect(surface).toHaveCSS("background-color", "rgb(248, 247, 250)");
    await theme.selectOption("dark");
    await page.emulateMedia({ colorScheme: "light" });
    await expect(surface).toHaveCSS("background-color", "rgb(16, 17, 19)");
    await theme.selectOption("system");
    await expect(html).toHaveAttribute("data-theme", "system");
    await expect(surface).toHaveCSS("background-color", "rgb(248, 247, 250)");
    await page.reload();
    await expect(theme).toHaveValue("system");
    await page.getByRole("link", { name: "Настройки", exact: true }).click();
    for (const width of [375, 1440]) {
      await page.setViewportSize({ width, height: 900 });
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
      await page.screenshot({ path: test.info().outputPath(`light-${width}.png`) });
    }
  });

  test("theme works on login when storage is denied", async ({ page }) => {
    await page.addInitScript(() => {
      Object.defineProperty(window, "localStorage", { get() { throw new DOMException("Storage denied", "SecurityError"); } });
    });
    await page.route("**/v1/session", (route) => route.fulfill({ status: 401, json: { error: "unauthorized" } }));
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/");
    await expect(page.getByRole("heading", { name: "Вход в платформу" })).toBeVisible();
    const theme = page.getByRole("combobox", { name: "Тема интерфейса" });
    await expect(theme).toHaveValue("system");
    await theme.selectOption("light");
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
    await expect(page.locator('[data-slot="app-surface"]')).toHaveCSS("background-color", "rgb(248, 247, 250)");
    await expect(theme).toHaveCSS("background-color", "rgb(248, 247, 250)");
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
    await page.screenshot({ path: test.info().outputPath("light-login-mobile.png") });
  });

  test("shared terminal theme keeps decoration subdued and navigation accessible", async ({ page }) => {
    await page.goto("/");
    const surface = page.locator('[data-slot="app-surface"]');
    await expect(surface).toHaveCSS("background-color", "rgb(16, 17, 19)");
    const ambient = page.locator('[data-slot="ambient"]');
    await expect(ambient).toHaveAttribute("aria-hidden", "true");
    await expect(ambient).toHaveCSS("pointer-events", "none");
    expect(Number(await ambient.evaluate((element) => getComputedStyle(element).opacity))).toBeCloseTo(1 / 3, 3);
    const nav = page.getByRole("navigation", { name: "Основная навигация" });
    await expect(nav.locator("a button")).toHaveCount(0);
    await expect(nav.getByRole("link", { name: "Обзор", exact: true })).toHaveAttribute("aria-current", "page");
    const settings = nav.getByRole("link", { name: "Настройки", exact: true });
    await settings.focus();
    await page.keyboard.press("Enter");
    await expect(page).toHaveURL(/\/settings$/);
    await expect(settings).toHaveAttribute("aria-current", "page");
    for (const width of [375, 768, 1440]) {
      await page.setViewportSize({ width, height: 900 });
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
      await page.screenshot({ path: test.info().outputPath(`theme-${width}.png`) });
    }
  });

  test("login uses the same theme and library controls", async ({ page }) => {
    await page.route("**/v1/session", (route) => route.fulfill({ status: 401, json: { error: "unauthorized" } }));
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/");
    await expect(page.getByRole("heading", { name: "Вход в платформу" })).toBeVisible();
    await expect(page.locator('[data-slot="app-surface"]')).toHaveCSS("background-color", "rgb(16, 17, 19)");
    await expect(page.getByLabel("API-ключ", { exact: true })).toHaveAttribute("data-slot", "input");
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
    await page.screenshot({ path: test.info().outputPath("login-mobile.png") });
  });

  test("overview page renders navigation and content", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByRole("heading", { name: "Обзор" })).toBeVisible();
    await expect(page.getByText("Сервис данных: не готов", { exact: true })).toBeVisible();
    await expect(page.getByRole("link", { name: "Настройки", exact: true })).toBeVisible();
  });
  test("navigation leads to the settings page", async ({ page }) => {
    await page.goto("/");
    await page.getByRole("link", { name: "Настройки", exact: true }).click();
    await expect(page.getByRole("heading", { name: "Настройки" })).toBeVisible();
    await expect(page).toHaveURL(/\/settings$/);
  });

  test("overview refresh and header use the same actual API status", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByText("Сервис данных: не готов", { exact: true })).toBeVisible();
    await page.route("**/v1/status", (route) => route.fulfill({ json: { platform: "ready", marketData: "unreachable" } }));
    await page.getByRole("button", { name: "Обновить состояние" }).click();
    await expect(page.getByText("Сервис данных: недоступен", { exact: true })).toBeVisible();
    await expect(page.getByText("недоступен", { exact: true })).toBeVisible();
    await page.route("**/v1/status", (route) => route.fulfill({ status: 503, json: {} }));
    await page.getByRole("button", { name: "Обновить состояние" }).click();
    await expect(page.getByRole("alert")).toContainText("Платформа недоступна (HTTP 503)");
    await expect(page.getByText("Сервис данных: статус недоступен", { exact: true })).toBeVisible();
  });

  test("setups page shows an explicit degraded or empty state", async ({ page }) => {
    await page.goto("/setups");
    await expect(page.getByRole("heading", { name: "Сетапы" })).toBeVisible();
    await expect(page.getByText("Сетапы не найдены.")).toBeVisible();
  });

  test("runs page shows the launch form", async ({ page }) => {
    await page.goto("/runs");
    await expect(page.getByRole("heading", { name: "Исторические запуски" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Запустить" })).toBeVisible();
  });

  test("mobile history shows explicit empty coverage without automatic jobs", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    const writes: string[] = [];
    page.on("request", (request) => { if (request.method() !== "GET") writes.push(request.url()); });
    await page.goto("/");
    await page.getByRole("link", { name: "История и сбор", exact: true }).click();
    await expect(page).toHaveURL(/\/history$/);
    await page.getByLabel("Инструмент БД").selectOption("1");
    await expect(page.getByText("В БД нет свечей этого инструмента.")).toBeVisible();
    await expect(page.getByText("Заданий загрузки пока нет.")).toBeVisible();
    expect(writes).toEqual([]);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  });

  test("mobile replay form validates dates and submits the selected algorithm", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/runs");
    await expect(page.getByRole("checkbox", { name: "BTCUSDT (Trading)" })).toBeChecked();
    await page.getByLabel("Начало", { exact: true }).fill("2026-10-10T13:00");
    await page.getByLabel("Конец", { exact: true }).fill("2026-10-10T12:00");
    await page.getByRole("button", { name: "Запустить", exact: true }).click();
    await expect(page.getByRole("alert")).toContainText("конец диапазона должен быть позже начала");
    await page.getByLabel("Конец", { exact: true }).fill("2026-10-10T14:00");
    const request = page.waitForRequest((candidate) => candidate.url().endsWith("/v1/replay-runs") && candidate.method() === "POST");
    await page.getByRole("button", { name: "Запустить", exact: true }).click();
    const body: unknown = (await request).postDataJSON();
    expect(body).toMatchObject({ async: true, algorithmId: "price_volume_spike", instruments: [{ symbol: "BTCUSDT" }] });
    await expect(page.getByRole("status")).toContainText("Запрос запуска принят");
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  });
});
