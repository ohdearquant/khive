import { Buffer } from "node:buffer";
import { expect, test } from "@playwright/test";

import { demoReviewFixture } from "../src/lib/fixtures/demo-review";

test("uses one keyboard path across the showcase, review views, and history rows", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByRole("button", { name: "Open command palette" }))
    .toBeVisible();
  await page.keyboard.press("Control+K");
  await page.keyboard.type("KG review");
  await expect(page.getByRole("option", { name: /KG review/i })).toBeVisible();
  await page.keyboard.press("Enter");
  await expect.poll(() => new URL(page.url()).pathname).toBe("/review");

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
  await expect.poll(() => new URL(page.url()).pathname).toBe("/");
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

test("switching to KG review keeps imported decisions and conversation state in memory", async ({ page }) => {
  await page.goto("/review");
  await page.locator('input[type="file"]').setInputFiles({
    name: "imported-review.json",
    mimeType: "application/json",
    buffer: Buffer.from(JSON.stringify(demoReviewFixture)),
  });
  await expect(page.getByText("Imported bundle · no writes")).toBeVisible();
  await page.getByRole("button", { name: "Request changes" }).click();
  await expect(page.getByText("Local decision: changes requested")).toBeVisible();

  await page.getByRole("textbox", { name: "Review comment" }).fill("Saved local note");
  await page.getByRole("button", { name: "Add local note" }).click();
  await page.getByRole("textbox", { name: "Review comment" }).fill("Draft survives a view switch");
  await page.getByRole("tab", { name: /^Activity/i }).click();
  await expect(page.locator("[data-review-activity-timeline]")).toBeVisible();
  await page.keyboard.press("Control+K");
  await page.getByRole("combobox", { name: "Search review commands" }).fill("KG review");
  await page.keyboard.press("Enter");

  await expect.poll(() => new URL(page.url()).pathname).toBe("/review");
  await expect(page.locator(".review-surface")).toHaveAttribute("aria-label", "Changes");
  await expect(page.getByText("Imported bundle · no writes")).toBeVisible();
  await expect(page.getByText("Local decision: changes requested")).toBeVisible();
  await expect(page.getByText("Saved local note")).toBeVisible();
  await expect(page.getByRole("textbox", { name: "Review comment" }))
    .toHaveValue("Draft survives a view switch");

  await page.keyboard.press("Control+K");
  await page.getByRole("combobox", { name: "Search review commands" }).fill("Repository showcase");
  const crossSurface = page.getByRole("option", { name: /Repository showcase/i });
  await expect(crossSurface).toBeDisabled();
  await expect(crossSurface).toContainText("Unavailable while this review has unsaved local state.");
  await page.keyboard.press("Enter");
  await expect.poll(() => new URL(page.url()).pathname).toBe("/review");
  await expect(page.getByRole("textbox", { name: "Review comment" }))
    .toHaveValue("Draft survives a view switch");
});
