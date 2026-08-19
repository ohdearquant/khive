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
  await page.getByLabel("Module search results")
    .getByRole("button", { name: `Inspect ${writer}` }).click();
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

test("navigates from module to analysis using only the command palette", async ({ page }) => {
  const writer = "crates/khive-db/src/writer_task.rs";
  await page.goto("/");
  await expect(page.getByRole("button", { name: "Open command palette" }))
    .toBeVisible();

  await page.keyboard.press("Control+K");
  const palette = page.getByRole("dialog", { name: "Repository commands" });
  await expect(palette).toBeVisible();
  const query = palette.getByRole("combobox", {
    name: "Search repository commands",
  });
  await query.fill(writer);
  await page.keyboard.press("Enter");

  const inspector = page.locator("[data-module-inspector]");
  await expect(inspector.getByRole("heading", { level: 3 })).toHaveText(writer);
  await expect(page).toHaveURL(new RegExp(`module=${encodeURIComponent(writer)}`));

  await page.keyboard.press("Control+K");
  const apiSurface = palette.getByRole("option", {
    name: /De-facto API surface/i,
  });
  for (let step = 0; step < 8; step += 1) {
    await page.keyboard.press("ArrowDown");
  }
  await expect(apiSurface).toHaveAttribute("aria-selected", "true");
  const [optionBox, resultsBox] = await Promise.all([
    apiSurface.boundingBox(),
    palette.getByRole("listbox", {
      name: "Repository command results",
    }).boundingBox(),
  ]);
  expect(optionBox).not.toBeNull();
  expect(resultsBox).not.toBeNull();
  expect(optionBox!.y).toBeGreaterThanOrEqual(resultsBox!.y);
  expect(optionBox!.y + optionBox!.height)
    .toBeLessThanOrEqual(resultsBox!.y + resultsBox!.height);
  await page.keyboard.press("Enter");

  await expect(page.locator('[data-view-id="api_surface"]')).toHaveAttribute(
    "aria-current",
    "page",
  );
  await expect(page).toHaveURL(/view=api_surface/);
  await expect(page).toHaveURL(new RegExp(`module=${encodeURIComponent(writer)}`));
  await expect(page.locator("[data-repository-dashboard]")).toBeFocused();
});

test("drills from analysis results into the shared inspector and browser history", async ({ page }) => {
  await page.goto("/");
  const panel = page.locator(".repo-view-panel");
  const inspector = page.locator("[data-module-inspector]");

  await page.locator('[data-view-id="api_surface"]').click();
  const apiResult = panel.locator(
    'button[aria-label^="Inspect "][aria-pressed="false"]',
  ).first();
  const apiLabel = await apiResult.getAttribute("aria-label");
  const apiPath = apiLabel?.replace(/^Inspect /, "");
  expect(apiPath).toBeTruthy();
  await apiResult.press("Enter");
  await expect(inspector).toBeFocused();
  await expect(inspector.getByRole("heading", { level: 3 })).toHaveText(apiPath!);
  await expect(page).toHaveURL(/view=api_surface/);
  await expect(page).toHaveURL(new RegExp(`module=${encodeURIComponent(apiPath!)}`));

  await page.locator('[data-view-id="structure_treemap"]').click();
  const treemapResult = panel.locator(
    'button[aria-label^="Inspect "][aria-pressed="false"]',
  ).first();
  const treemapLabel = await treemapResult.getAttribute("aria-label");
  const treemapPath = treemapLabel?.replace(/^Inspect /, "");
  expect(treemapPath).toBeTruthy();
  await treemapResult.press("Enter");
  await expect(inspector).toBeFocused();
  await expect(inspector.getByRole("heading", { level: 3 })).toHaveText(
    treemapPath!,
  );
  await expect(page).toHaveURL(/view=structure_treemap/);

  await page.goBack();
  await expect(page.locator('[data-view-id="structure_treemap"]')).toHaveAttribute(
    "aria-current",
    "page",
  );
  await expect(inspector.getByRole("heading", { level: 3 })).toHaveText(apiPath!);
  await page.goBack();
  await expect(page.locator('[data-view-id="api_surface"]')).toHaveAttribute(
    "aria-current",
    "page",
  );
  await expect(inspector.getByRole("heading", { level: 3 })).toHaveText(apiPath!);
});

test("shares structure graph module selection with browser history", async ({ page }) => {
  await page.goto("/");
  const inspector = page.locator("[data-module-inspector]");
  const graph = page.locator(".repo-graph-stage");
  const moduleNodes = graph.locator(
    '.repo-graph-node[aria-controls="repository-module-inspector"]',
  );
  await expect(moduleNodes.first()).toBeVisible();

  const unselectedModuleNodes = graph.locator(
    '.repo-graph-node[aria-controls="repository-module-inspector"][aria-pressed="false"]',
  );
  const firstCandidate = unselectedModuleNodes.first();
  const firstLabel = await firstCandidate.getAttribute("aria-label");
  const firstPath = firstLabel?.replace(/^Inspect /, "");
  expect(firstPath).toBeTruthy();
  const first = graph.getByRole("button", { name: firstLabel!, exact: true });
  await first.press("Enter");

  await expect(first).toHaveAttribute("aria-pressed", "true");
  await expect(inspector).toBeFocused();
  await expect(inspector.getByRole("heading", { level: 3 })).toHaveText(
    firstPath!,
  );
  await expect(page).toHaveURL(/view=structure_graph/);
  await expect(page).toHaveURL(
    new RegExp(`module=${encodeURIComponent(firstPath!)}`),
  );

  const secondCandidate = unselectedModuleNodes.first();
  const secondLabel = await secondCandidate.getAttribute("aria-label");
  const secondPath = secondLabel?.replace(/^Inspect /, "");
  expect(secondPath).toBeTruthy();
  const second = graph.getByRole("button", { name: secondLabel!, exact: true });
  await second.click();
  await expect(second).toHaveAttribute("aria-pressed", "true");
  await expect(inspector.getByRole("heading", { level: 3 })).toHaveText(
    secondPath!,
  );

  await page.goBack();
  await expect(page).toHaveURL(
    new RegExp(`module=${encodeURIComponent(firstPath!)}`),
  );
  await expect(inspector.getByRole("heading", { level: 3 })).toHaveText(
    firstPath!,
  );
  await expect(first).toHaveAttribute("aria-pressed", "true");
  await expect(second).toHaveAttribute("aria-pressed", "false");
});

test("uses the structure lens to verify a hidden khive-db boundary", async ({ page }) => {
  const graphImplementation = "crates/khive-db/src/stores/graph.rs";
  const graphTests = "crates/khive-db/src/stores/graph_tests.rs";
  await page.goto("/");

  const toolbar = page.locator(".repo-graph-toolbar");
  await toolbar.getByRole("combobox", { name: /Package · Structure graph/ })
    .selectOption({ label: "khive-db" });
  await expect(page).toHaveURL(/pkg=khive-db/);
  await toolbar.getByRole("radio", { name: "Hidden coupling" }).check();
  await expect(page).toHaveURL(/lens=hidden_coupling/);

  const lens = page.getByRole("region", { name: "Hidden coupling lens" });
  await expect(lens).toContainText("20 of 70 captured visible pairs shown");
  await expect(lens).toContainText("365-day analysis window");
  await expect(lens).toContainText("1,000 captured of 104,263 declared");
  await expect(page.locator("[data-coupling-overlay]")).toHaveCount(20);

  const graphPair = lens.getByRole("button", {
    name:
      "Focus coupling candidate between crates/khive-db/src/stores/graph_tests.rs and crates/khive-db/src/stores/graph.rs",
  });
  await graphPair.press("Enter");
  expect(new URL(page.url()).searchParams.getAll("pair")).toEqual([
    graphImplementation,
    graphTests,
  ]);
  await expect(lens).toContainText("No captured direct dependency edge");
  await expect(page.locator("[data-coupling-overlay].selected")).toHaveCount(1);
  expect(await page.locator(".repo-graph-node.context-dimmed").count())
    .toBeGreaterThan(0);

  await graphPair.locator("..").getByRole("button", {
    name: `Inspect ${graphImplementation}`,
  })
    .press("Enter");
  await expect(page.locator("[data-module-inspector]")).toBeFocused();
  await expect(page.locator("[data-module-inspector]").getByRole("heading", {
    level: 3,
  })).toHaveText(graphImplementation);
  await expect(page).toHaveURL(/view=structure_graph/);
  await expect(page).toHaveURL(
    new RegExp(`module=${encodeURIComponent(graphImplementation)}`),
  );
  expect(new URL(page.url()).searchParams.getAll("pair")).toEqual([
    graphImplementation,
    graphTests,
  ]);

  await page.reload();
  await expect(toolbar.getByRole("combobox", {
    name: /Package · Structure graph/,
  }).locator("option:checked")).toHaveText("khive-db");
  await expect(toolbar.getByRole("radio", { name: "Hidden coupling" }))
    .toBeChecked();
  await expect(page.locator("[data-coupling-overlay].selected")).toHaveCount(1);
  await expect(page.locator("[data-module-inspector]").getByRole("heading", {
    level: 3,
  })).toHaveText(graphImplementation);

  await page.goBack();
  await expect(page.locator("[data-coupling-overlay].selected")).toHaveCount(1);
  await page.goBack();
  await expect(page).not.toHaveURL(/pair=/);
  await expect(page.locator("[data-coupling-overlay].selected")).toHaveCount(0);
  await page.goBack();
  await expect(toolbar.getByRole("radio", { name: "Structure graph" }))
    .toBeChecked();
  await expect(page).not.toHaveURL(/lens=/);
});

test("keeps the structure inspector legible across desktop and mobile", async ({ page }) => {
  await page.goto("/");

  await page.getByRole("combobox", { name: /Package · Structure graph/ })
    .selectOption({ label: "khive-db" });
  await page.getByRole("button", {
    name: "Inspect crates/khive-db/src/stores/graph.rs",
    exact: true,
  }).click();

  const heading = page.locator(".repo-inspector-heading");
  await expect(heading.getByText("stores::graph", { exact: true })).toBeVisible();
  await expect(heading.getByText("crates/khive-db/src/stores/graph.rs", { exact: true }))
    .toBeVisible();

  const measure = () => heading.evaluate((element) => {
    const moduleName = element.querySelector("strong");
    const sourcePath = element.querySelector("code");
    if (!moduleName || !sourcePath) throw new Error("graph inspector heading is incomplete");
    return {
      moduleNameFits: moduleName.scrollWidth <= moduleName.clientWidth,
      sourcePathFits: sourcePath.scrollWidth <= sourcePath.clientWidth,
      sourcePathOverflowWrap: getComputedStyle(sourcePath).overflowWrap,
      documentFits: document.documentElement.scrollWidth === document.documentElement.clientWidth,
    };
  });

  expect(await measure()).toMatchObject({
    moduleNameFits: true,
    sourcePathFits: true,
    documentFits: true,
  });

  await page.setViewportSize({ width: 375, height: 812 });
  expect(await measure()).toEqual({
    moduleNameFits: true,
    sourcePathFits: true,
    sourcePathOverflowWrap: "anywhere",
    documentFits: true,
  });
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
