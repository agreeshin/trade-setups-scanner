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
});
