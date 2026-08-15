import { expect, test } from "@playwright/test";

test("dogfoods every repository analysis from the curated static bundle", async ({ page }) => {
  const consoleErrors: string[] = [];
  page.on("console", (message) => {
    if (message.type() === "error") consoleErrors.push(message.text());
  });
  await page.route("**/api/showcase/analyses/khive", async (route) => {
    await route.fulfill({ status: 404 });
  });

  await page.goto("/");
  const overview = page.locator(".repo-overview");
  await expect(overview).toBeVisible();
  await expect(overview).toHaveAttribute("data-head-sha", "c2979d2443738a075e55a170c772d1dc86cf0f91");

  await page.locator("#repository-url").fill("http://github.com/ohdearquant/khive.git");
  await page.locator(".repo-url-form button[type=submit]").click();
  await expect(page).toHaveURL(/repo=https%3A%2F%2Fgithub\.com%2Fohdearquant%2Fkhive/);
  await expect(overview).toHaveAttribute("data-head-sha", "c2979d2443738a075e55a170c772d1dc86cf0f91");
  await expect(page.locator('[data-view-id="structure_graph"]')).toHaveAttribute("aria-current", "page");

  const viewIds = [
    "structure_graph",
    "history_structure_navigation",
    "dependency_topology",
    "hotspot_quadrant",
    "hidden_coupling",
    "structure_treemap",
    "cadence_timeline",
    "ownership",
    "api_surface",
    "scorecard",
  ];
  await expect(page.locator("[data-view-id]")).toHaveCount(viewIds.length);

  const graphToolbar = page.locator(".repo-graph-toolbar");
  await graphToolbar.locator("button").last().click();
  await expect(graphToolbar.locator("output")).toHaveText("125%");
  await graphToolbar.locator("select").selectOption({ index: 1 });
  await expect(graphToolbar.locator("select")).not.toHaveValue(/repository/);

  await page.locator('[data-view-id="history_structure_navigation"]').click();
  const firstModule = page.locator("[data-history-modules] [data-module-id]").first();
  await expect(firstModule).toBeVisible();
  await firstModule.click();
  const firstCommit = page.locator("[data-history-commits] [data-commit-id]").first();
  await expect(firstCommit).toBeVisible();
  await firstCommit.click();
  await expect(firstCommit).toHaveClass(/selected/);
  await expect(page.locator("[data-history-modules] [data-module-id]").first()).toBeVisible();

  const joinResolution = page.locator("[data-join-resolution]");
  await expect(joinResolution).toContainText("100%");
  await expect(joinResolution.locator(".repo-residuals li").first()).toBeVisible();
  await expect(joinResolution.locator(".repo-bounded").last()).toContainText("35 / 35");

  await page.locator('[data-view-id="cadence_timeline"]').click();
  await expect(page.locator('[data-cadence-series="commits"]')).toHaveAttribute("data-series-status", "complete");
  for (const series of ["issues_opened", "issues_closed", "pull_requests_opened", "pull_requests_merged"]) {
    await expect(page.locator(`[data-cadence-series="${series}"]`)).toHaveAttribute("data-series-status", "unavailable");
  }

  for (const viewId of viewIds) {
    const trigger = page.locator(`[data-view-id="${viewId}"]`);
    await expect(trigger).toBeVisible();
    await trigger.click();
    await expect(trigger).toHaveAttribute("aria-current", "page");
    await expect(page.locator(".repo-view-panel h2")).not.toBeEmpty();
  }

  expect(consoleErrors).toEqual([
    expect.stringMatching(/failed to load resource.*404/i),
  ]);
});

test("restores a shared investigation and follows browser back and forward", async ({ page }) => {
  const snapshot = "c2979d2443738a075e55a170c772d1dc86cf0f91";
  const pool = "crates/khive-db/src/pool.rs";
  const writer = "crates/khive-db/src/writer_task.rs";
  const query = new URLSearchParams({
    repo: "https://github.com/ohdearquant/khive",
    at: snapshot,
    module: pool,
    view: "dependency_topology",
  });
  await page.goto(`/?${query.toString()}`);

  const inspector = page.locator("[data-module-inspector]");
  await expect(inspector.getByRole("heading", { level: 3 })).toHaveText(pool);
  await expect(page.locator('[data-view-id="dependency_topology"]')).toHaveAttribute(
    "aria-current",
    "page",
  );

  await page.getByRole("searchbox", { name: "Find a module or path" }).fill(writer);
  await page.getByRole("button", { name: `Inspect ${writer}` }).click();
  await expect(inspector.getByRole("heading", { level: 3 })).toHaveText(writer);
  await expect(page).toHaveURL(new RegExp(`module=${encodeURIComponent(writer)}`));

  await page.locator('[data-view-id="hidden_coupling"]').click();
  await expect(page).toHaveURL(/view=hidden_coupling/);

  await page.goBack();
  await expect(page).toHaveURL(/view=dependency_topology/);
  await expect(inspector.getByRole("heading", { level: 3 })).toHaveText(writer);

  await page.goBack();
  await expect(inspector.getByRole("heading", { level: 3 })).toHaveText(pool);
  await page.goForward();
  await expect(inspector.getByRole("heading", { level: 3 })).toHaveText(writer);
});

test("a valid repository miss stays local and renders an honest state", async ({ page }) => {
  const requestedAfterSubmit: string[] = [];
  let observing = false;
  page.on("request", (request) => {
    if (observing) requestedAfterSubmit.push(request.url());
  });
  await page.goto("/");
  await expect(page.locator(".repo-overview")).toBeVisible();

  await page.getByLabel("Public repository URL").fill("https://github.com/example/not-curated");
  observing = true;
  await page.locator(".repo-url-form button[type=submit]").click();

  await expect(page.locator(".repo-state-card[data-state='empty']")).toBeVisible();
  await page.waitForTimeout(100);
  expect(requestedAfterSubmit).toEqual([]);
});

test("preserves the semantic review workbench on its own route", async ({ page }) => {
  await page.goto("/review");
  await expect(page.getByText("Demo data · no writes")).toBeVisible();
  await expect(page.getByRole("heading", { name: /Curate assertion-level provenance/i })).toBeVisible();
});
