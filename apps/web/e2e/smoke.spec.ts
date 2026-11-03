import { expect, test } from "@playwright/test";
test.describe("application smoke", () => {
  test("overview page renders navigation and content", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByRole("heading", { name: "Обзор" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Настройки" })).toBeVisible();
  });
  test("navigation leads to the settings page", async ({ page }) => {
    await page.goto("/");
    await page.getByRole("button", { name: "Настройки" }).click();
    await expect(page.getByRole("heading", { name: "Настройки" })).toBeVisible();
    await expect(page).toHaveURL(/\/settings$/);
  });

  test("setups page shows an explicit degraded or empty state", async ({ page }) => {
    await page.goto("/setups");
    await expect(page.getByRole("heading", { name: "Сетапы" })).toBeVisible();
    const alert = page.getByRole("alert");
    await expect(alert.or(page.getByText("Нет сохранённых"))).toBeVisible();
  });

  test("runs page shows the launch form", async ({ page }) => {
    await page.goto("/runs");
    await expect(page.getByRole("heading", { name: "Исторические запуски" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Запустить" })).toBeVisible();
  });
});
