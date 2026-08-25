import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { RepoShowcase } from "@/components/showcase/repo-showcase";
import { parseRepoBundle, type RepoBundle } from "@/lib/repo-bundle";

const goldenPath = resolve(
  process.cwd(),
  "../../docs/schemas/examples/khive-repo-v1-khive.json",
);

function golden(): RepoBundle {
  return parseRepoBundle(JSON.parse(readFileSync(goldenPath, "utf8")));
}

describe("repository showcase polish", () => {
  beforeEach(() => {
    window.history.replaceState(null, "", "/");
  });

  it("renders scorecard units, a discriminating ownership tier, and compact unavailable cards", async () => {
    const bundle = golden();
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);

    await user.click(container.querySelector('[data-view-id="scorecard"]')!);

    const ageCard = screen
      .getByText(bundle.capability.labels.metrics.repository_age)
      .closest("article")!;
    expect(within(ageCard).getByText("61 days")).toBeVisible();

    const warningIds = new Set(
      bundle.aggregates.scorecard.fields.flatMap((field) =>
        field.key === "ownership_warnings" &&
        field.value.status === "available" &&
        field.value.value.value_kind === "module_ids"
          ? field.value.value.value.items
          : [],
      ),
    );
    const highImpactWarningCount =
      bundle.aggregates.hotspot_quadrant.data.items.filter(
        (row) =>
          row.quadrant === "high_churn_high_fan_in" &&
          warningIds.has(row.module_id),
      ).length;
    expect(highImpactWarningCount).toBeGreaterThan(0);
    expect(highImpactWarningCount).toBeLessThan(warningIds.size);
    const warningCard = screen
      .getByText(bundle.capability.labels.metrics.ownership_warnings)
      .closest("article")!;
    expect(
      within(warningCard).getByText(
        highImpactWarningCount.toLocaleString("en"),
      ),
    ).toBeVisible();
    expect(warningCard).toHaveTextContent("High churn · high fan-in tier");

    const symbolCard = screen
      .getByText(bundle.capability.labels.metrics.symbol_count)
      .closest("article")!;
    expect(symbolCard).toHaveClass("unavailable");
  });

  it("withholds the ownership tier when the hotspot page is incomplete", async () => {
    const raw = JSON.parse(readFileSync(goldenPath, "utf8"));
    // A cursor-bearing hotspot page may be missing exactly the modules the
    // tier filter selects; the tier claim must not survive that.
    raw.aggregates.hotspot_quadrant.data.next_cursor = "hotspot-cursor-1";
    const bundle = parseRepoBundle(raw);
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);

    await user.click(container.querySelector('[data-view-id="scorecard"]')!);

    const warningIds = new Set(
      bundle.aggregates.scorecard.fields.flatMap((field) =>
        field.key === "ownership_warnings" &&
        field.value.status === "available" &&
        field.value.value.value_kind === "module_ids"
          ? field.value.value.value.items
          : [],
      ),
    );
    const warningCard = screen
      .getByText(bundle.capability.labels.metrics.ownership_warnings)
      .closest("article")!;
    expect(warningCard).not.toHaveTextContent("High churn · high fan-in tier");
    expect(
      within(warningCard).getByText(warningIds.size.toLocaleString("en")),
    ).toBeVisible();
  });

  it("withholds the ownership tier when the hotspot analysis is unavailable", async () => {
    const raw = JSON.parse(readFileSync(goldenPath, "utf8"));
    // An unavailable analysis can still ship a schema-valid data page;
    // filtering against it would turn unknown coverage into a false zero
    // presented as a definitive tier.
    raw.aggregates.hotspot_quadrant.meta.status = "unavailable";
    raw.aggregates.hotspot_quadrant.meta.unavailable_reason =
      "hotspot analysis inputs missing from this export";
    // The capability view mirrors the aggregate status (schema cross-refine).
    raw.capability.views.hotspot_quadrant.status = "unavailable";
    raw.capability.views.hotspot_quadrant.unavailable_reason =
      "hotspot analysis inputs missing from this export";
    const bundle = parseRepoBundle(raw);
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);

    await user.click(container.querySelector('[data-view-id="scorecard"]')!);

    const warningCard = screen
      .getByText(bundle.capability.labels.metrics.ownership_warnings)
      .closest("article")!;
    expect(warningCard).not.toHaveTextContent("High churn · high fan-in tier");
    // The unfiltered warning count still renders (658 in the golden bundle).
    expect(within(warningCard).getByText("658")).toBeVisible();
  });

  it("reports the declared total, not the partial floor, for an incomplete ownership page", async () => {
    const raw = JSON.parse(readFileSync(goldenPath, "utf8"));
    const field = raw.aggregates.scorecard.fields.find(
      (candidate: { key: string }) => candidate.key === "ownership_warnings",
    );
    // Cursor-bearing page carrying only part of the ID list: the floor is
    // not the count, but the exporter's declared total still is.
    field.value.value.value.items = field.value.value.value.items.slice(0, 100);
    field.value.value.value.next_cursor = "ownership-cursor-1";
    const bundle = parseRepoBundle(raw);
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);

    await user.click(container.querySelector('[data-view-id="scorecard"]')!);

    const warningCard = screen
      .getByText(bundle.capability.labels.metrics.ownership_warnings)
      .closest("article")!;
    expect(warningCard).not.toHaveTextContent("High churn · high fan-in tier");
    expect(within(warningCard).getByText("658")).toBeVisible();
    expect(within(warningCard).queryByText("100")).toBeNull();
  });

  it("marks the count as a floor when an incomplete ownership page has no declared total", async () => {
    const raw = JSON.parse(readFileSync(goldenPath, "utf8"));
    const field = raw.aggregates.scorecard.fields.find(
      (candidate: { key: string }) => candidate.key === "ownership_warnings",
    );
    field.value.value.value.items = field.value.value.value.items.slice(0, 100);
    field.value.value.value.next_cursor = "ownership-cursor-1";
    field.value.value.value.total_count = {
      status: "unavailable",
      reason: "total not exported",
    };
    const bundle = parseRepoBundle(raw);
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);

    await user.click(container.querySelector('[data-view-id="scorecard"]')!);

    const warningCard = screen
      .getByText(bundle.capability.labels.metrics.ownership_warnings)
      .closest("article")!;
    expect(warningCard).not.toHaveTextContent("High churn · high fan-in tier");
    expect(within(warningCard).getByText("100+")).toBeVisible();
  });

  it("sorts dependency rows before the display cap and distinguishes the API bar header", async () => {
    const bundle = golden();
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);
    const expectedFirst = [
      ...bundle.aggregates.dependency_topology.modules.items,
    ].sort(
      (left, right) =>
        right.fan_in - left.fan_in ||
        right.cycle_ids.length - left.cycle_ids.length ||
        right.fan_out - left.fan_out ||
        left.module_id.localeCompare(right.module_id),
    )[0];

    await user.click(
      container.querySelector('[data-view-id="dependency_topology"]')!,
    );
    let panel = container.querySelector<HTMLElement>(".repo-view-panel")!;
    const dependencyRows = within(panel).getAllByRole("row").slice(1);
    expect(dependencyRows).toHaveLength(200);
    expect(
      dependencyRows[0].querySelector<HTMLElement>("[data-module-id]")?.dataset
        .moduleId,
    ).toBe(expectedFirst.module_id);

    await user.click(container.querySelector('[data-view-id="api_surface"]')!);
    panel = container.querySelector<HTMLElement>(".repo-view-panel")!;
    expect(
      within(panel)
        .getAllByRole("columnheader")
        .map((header) => header.textContent),
    ).toEqual([
      bundle.capability.labels.node_types.module,
      bundle.capability.labels.metrics.dependent_count,
      "Relative magnitude",
    ]);
  });

  it("explains missing history facets and discloses the 100-module browser cap", async () => {
    const bundle = structuredClone(golden());
    const user = userEvent.setup();
    bundle.graph.history_navigation.by_module.items = [];
    const { container } = render(<RepoShowcase bundle={bundle} />);

    await user.click(
      container.querySelector('[data-view-id="history_structure_navigation"]')!,
    );

    for (const label of [
      bundle.capability.labels.node_types.pull_request,
      bundle.capability.labels.node_types.issue,
    ]) {
      const card = within(
        container.querySelector<HTMLElement>(".repo-view-panel")!,
      )
        .getByRole("heading", { level: 3, name: label })
        .closest("section")!;
      // These fixture pages disclose unavailable bounds, so their zero rows
      // are unknown, not proven absent: the definitive empty label is
      // reserved for complete, cursor-free pages.
      expect(card).toHaveTextContent(`${label} capture incomplete`);
      expect(card).not.toHaveTextContent(`No ${label} navigation captured`);
    }

    const moduleCard = container.querySelector<HTMLElement>(
      "[data-history-modules]",
    )!;
    const disclosure = moduleCard.querySelector<HTMLElement>(
      '[data-state="truncated"][data-shown="100"]',
    );
    expect(disclosure).toBeVisible();
    expect(disclosure).toHaveAttribute(
      "data-known-total",
      String(bundle.graph.modules.items.length),
    );
  });

  it("visually separates the showcase title from its source mode", () => {
    const bundle = golden();
    const { container } = render(<RepoShowcase bundle={bundle} />);

    expect(
      container.querySelector(".repo-capability-strip > div"),
    ).toHaveTextContent(
      `${bundle.capability.labels.product} · ${bundle.capability.mode}`,
    );
  });
});
