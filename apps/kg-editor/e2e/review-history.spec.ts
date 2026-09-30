import { expect, test, type Locator, type Page } from "@playwright/test";

import { demoReviewFixture } from "../src/lib/fixtures/demo-review";
import { reviewActivityEvents } from "../src/lib/review-activity";

async function tabTo(page: Page, target: Locator) {
  for (let index = 0; index < 120; index += 1) {
    await page.keyboard.press("Tab");
    if (await target.evaluate((element) => document.activeElement === element)) return;
  }
  throw new Error("Keyboard Tab traversal did not reach the target control.");
}

test("keyboard review walk restores unit, timeline event, and graph selection with Back and Forward", async ({ page }) => {
  const firstChange = demoReviewFixture.changes.items[0];
  const secondChange = demoReviewFixture.changes.items[1];
  const firstNode = demoReviewFixture.graph.nodes.items[0];
  const firstEdge = demoReviewFixture.graph.edges.items[0];
  const firstUnit = `${firstChange.substrate}:${firstChange.id}`;
  const secondUnit = `${secondChange.substrate}:${secondChange.id}`;
  const activityEvents = reviewActivityEvents(demoReviewFixture);
  const initialEvent = activityEvents[0].id;
  const evidenceEvents = activityEvents.filter((event) => event.kind === "evidence");
  const evidenceIndex = evidenceEvents.findIndex((event) => event.id !== initialEvent);
  expect(evidenceIndex).toBeGreaterThanOrEqual(0);
  const evidenceEvent = evidenceEvents[evidenceIndex].id;

  await page.goto("/review");
  await expect(page.getByRole("tab")).toHaveCount(4);
  await expect.poll(() => new URL(page.url()).searchParams.get("view")).toBe("changes");
  await expect.poll(() => new URL(page.url()).searchParams.get("unit")).toBe(firstUnit);
  await expect(page.locator("[data-review-thread]")).toContainText(firstChange.id);

  const firstRow = page.locator("[data-review-unit-row]").first();
  const secondRow = page.locator("[data-review-unit-row]").nth(1);
  await tabTo(page, firstRow);
  await page.keyboard.press("j");
  await expect(secondRow).toBeFocused();
  await page.keyboard.press("Enter");
  await expect.poll(() => new URL(page.url()).searchParams.get("unit")).toBe(secondUnit);
  await expect(secondRow).toHaveAttribute("aria-pressed", "true");
  await expect(page.locator("[data-review-thread]")).toContainText(secondChange.id);

  const activityTab = page.getByRole("tab", { name: /^Activity/ });
  await tabTo(page, activityTab);
  await page.keyboard.press("Enter");
  await expect.poll(() => new URL(page.url()).searchParams.get("view")).toBe("activity");
  await expect.poll(() => new URL(page.url()).searchParams.get("event")).toBe(initialEvent);
  const timeline = page.locator("[data-review-activity-timeline]");
  await expect(timeline).toBeVisible();
  await expect(timeline.getByRole("group", { name: "Filter history by type" }).getByRole("button")).toHaveCount(6);
  const allCount = await timeline.locator("[data-activity-kind]").count();
  expect(allCount).toBeGreaterThan(1);
  const beforeFacetUrl = page.url();
  const evidenceFacet = timeline.getByRole("button", { name: "Evidence", exact: true });
  await tabTo(page, evidenceFacet);
  await page.keyboard.press("Enter");
  await expect(evidenceFacet).toHaveAttribute("aria-pressed", "true");
  await expect(timeline.locator('[data-activity-kind="evidence"]')).toHaveCount(demoReviewFixture.evidence.items.length);
  await expect(timeline.locator('[data-activity-kind]:not([data-activity-kind="evidence"])')).toHaveCount(0);
  expect(page.url()).toBe(beforeFacetUrl);

  const beforeEventSelectionUrl = page.url();
  const selectedEvidenceRow = timeline.locator('[data-activity-kind="evidence"] [data-keyboard-row]').nth(evidenceIndex);
  await tabTo(page, selectedEvidenceRow);
  await page.keyboard.press("Enter");
  await expect.poll(() => new URL(page.url()).searchParams.get("event")).toBe(evidenceEvent);
  expect(page.url()).not.toBe(beforeEventSelectionUrl);
  await expect(selectedEvidenceRow).toHaveAttribute("aria-pressed", "true");
  const allFacet = timeline.getByRole("button", { name: "All activity" });
  await tabTo(page, allFacet);
  await page.keyboard.press("Enter");
  await expect(timeline.locator("[data-activity-kind]")).toHaveCount(allCount);
  await expect(timeline.locator('[data-activity-kind="conversation"]')).not.toHaveCount(0);
  expect(new URL(page.url()).searchParams.get("view")).toBe("activity");

  const graphTab = page.getByRole("tab", { name: /^Graph/ });
  await tabTo(page, graphTab);
  await page.keyboard.press("Enter");
  await expect.poll(() => new URL(page.url()).searchParams.get("view")).toBe("graph");
  await expect.poll(() => new URL(page.url()).searchParams.get("node")).toBe(firstNode.id);
  await expect(page.locator(".graph-node.selected")).toBeVisible();

  const edgeRow = page.getByRole("region", { name: "Affected graph relationships" }).getByRole("button").first();
  await tabTo(page, edgeRow);
  await page.keyboard.press("Enter");
  await expect.poll(() => new URL(page.url()).searchParams.get("edge")).toBe(firstEdge.id);
  await expect(page.locator(".edge-inspector")).toBeVisible();

  await page.goBack();
  await expect.poll(() => new URL(page.url()).searchParams.get("node")).toBe(firstNode.id);
  await expect(page.locator(".edge-inspector")).toHaveCount(0);

  await page.goBack();
  await expect.poll(() => new URL(page.url()).searchParams.get("view")).toBe("activity");
  await expect.poll(() => new URL(page.url()).searchParams.get("event")).toBe(evidenceEvent);
  await expect(page.locator("[data-review-activity-timeline]")).toBeVisible();

  await page.goBack();
  await expect(page).toHaveURL(beforeEventSelectionUrl);
  await expect.poll(() => new URL(page.url()).searchParams.get("event")).toBe(initialEvent);

  await page.goBack();
  await expect.poll(() => new URL(page.url()).searchParams.get("view")).toBe("changes");
  await expect.poll(() => new URL(page.url()).searchParams.get("unit")).toBe(secondUnit);
  await expect(page.locator("[data-review-thread]")).toContainText(secondChange.id);

  await page.goBack();
  await expect.poll(() => new URL(page.url()).searchParams.get("unit")).toBe(firstUnit);
  await expect(page.locator("[data-review-thread]")).toContainText(firstChange.id);

  await page.goForward();
  await expect.poll(() => new URL(page.url()).searchParams.get("unit")).toBe(secondUnit);
  await page.goForward();
  await expect.poll(() => new URL(page.url()).searchParams.get("view")).toBe("activity");
  await page.goForward();
  await expect.poll(() => new URL(page.url()).searchParams.get("event")).toBe(evidenceEvent);
  await page.goForward();
  await expect.poll(() => new URL(page.url()).searchParams.get("view")).toBe("graph");
  await expect(page.locator(".pr-header .breadcrumb > *")).toHaveCount(7);
});

test("a deep link opens the addressed graph edge", async ({ page }) => {
  const firstEdge = demoReviewFixture.graph.edges.items[0];
  await page.goto(`/review?view=graph&edge=${encodeURIComponent(firstEdge.id)}`);
  await expect(page.locator(".review-surface")).toHaveAttribute("aria-label", "Affected graph");
  await expect(page.locator(".edge-inspector")).toBeVisible();
});
