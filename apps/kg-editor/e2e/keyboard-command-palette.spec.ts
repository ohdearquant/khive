import { expect, test } from "@playwright/test";

test("uses one keyboard path across the showcase, review views, and history rows", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByRole("button", { name: "Open command palette" }))
    .toBeVisible();
  await page.keyboard.press("Control+K");
  await page.keyboard.type("KG review");
  await expect(page.getByRole("option", { name: /KG review/i })).toBeVisible();
  await page.keyboard.press("Enter");
  await expect(page).toHaveURL(/\/review\/?$/);

  await page.keyboard.press("Control+K");
  await expect(page.getByRole("dialog", { name: "Review commands" }))
    .toBeVisible();
  await page.keyboard.type("Activity");
  await page.keyboard.press("Enter");
  await expect(page.locator(".review-surface"))
    .toHaveAttribute("aria-label", "Activity");

  await page.keyboard.press("Control+K");
  await page.keyboard.type("Repository showcase");
  await page.keyboard.press("Enter");
  await expect(page).toHaveURL(/\/$/);
  await expect(page.getByRole("button", { name: "Open command palette" }))
    .toBeVisible();
  await page.keyboard.press("Control+K");
  await page.keyboard.type("History-structure navigation");
  await page.keyboard.press("Enter");
  const rows = page.locator("[data-history-modules] [data-keyboard-row]");
  await expect(rows.first()).toBeVisible();
  await page.keyboard.press("j");
  await expect(rows.first()).toBeFocused();
  await page.keyboard.press("j");
  await expect(rows.nth(1)).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(rows.nth(1)).toHaveAttribute("aria-pressed", "true");
});
