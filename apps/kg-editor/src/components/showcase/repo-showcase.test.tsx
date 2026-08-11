import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { RepoShowcase } from "@/components/showcase/repo-showcase";
import { parseRepoBundle, type RepoBundle } from "@/lib/repo-bundle";

const goldenPath = resolve(process.cwd(), "../../docs/schemas/examples/khive-repo-v1-khive.json");

function golden(): RepoBundle {
  return parseRepoBundle(JSON.parse(readFileSync(goldenPath, "utf8")));
}

function exactish(value: string): RegExp {
  return new RegExp(value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), "i");
}

describe("repository showcase", () => {
  it("renders all ten capability-owned view labels", () => {
    const bundle = golden();
    render(<RepoShowcase bundle={bundle} />);

    const navigation = screen.getByRole("navigation", { name: bundle.capability.labels.product });
    for (const view of Object.values(bundle.capability.views)) {
      expect(within(navigation).getByRole("button", { name: view.label })).toBeVisible();
    }
  });

  it("uses the shared ontology legend and marks exporter-derived edges geometrically", () => {
    const { container } = render(<RepoShowcase bundle={golden()} />);

    expect(screen.getByLabelText("Ontology legend")).toHaveTextContent(/Project.*Concept.*Contains.*Derived/i);
    expect(container.querySelector('line[data-edge-origin="derived"]')).toHaveAttribute("marker-end", "url(#showcase-ontology-arrow)");
    expect(container.querySelector(".ontology-direction-glyph")?.getAttribute("transform")).toMatch(/^rotate\(/);
    expect(container.querySelector(".ontology-derived-glyph")).toHaveTextContent("◇");
  });

  it("navigates from a module to its precomputed commits and back to modules", async () => {
    const bundle = golden();
    const user = userEvent.setup();
    const moduleNavigation = bundle.graph.history_navigation.by_module.items.find((item) => item.commits.items.length > 0);
    expect(moduleNavigation).toBeDefined();
    const moduleNode = bundle.graph.modules.items.find((item) => item.id === moduleNavigation?.module_id);
    const commitId = moduleNavigation?.commits.items[0];
    const commit = bundle.graph.commits.items.find((item) => item.id === commitId);
    expect(moduleNode).toBeDefined();
    expect(commit).toBeDefined();

    const { container } = render(<RepoShowcase bundle={bundle} />);
    await user.click(screen.getByRole("button", { name: bundle.capability.views.history_structure_navigation.label }));
    await user.click(container.querySelector(`[data-module-id="${moduleNode!.id}"]`)!);
    const commitButton = screen.getByRole("button", { name: exactish(commit!.subject) });
    expect(commitButton).toBeVisible();
    await user.click(commitButton);
    expect(container.querySelector(`[data-module-id="${moduleNode!.id}"]`)).toBeVisible();
    expect(screen.getAllByText(bundle.capability.labels.derived).length).toBeGreaterThan(0);
  });

  it("keeps known-empty history distinct from unavailable and preserves available false", async () => {
    const bundle = structuredClone(golden());
    const user = userEvent.setup();
    const first = bundle.graph.history_navigation.by_module.items[0];
    first.commits = {
      ...first.commits,
      items: [],
      total_count: { status: "available", value: 0 },
      next_cursor: null,
      truncated: false,
      disclosure: { status: "complete", reason: null },
    };
    bundle.capability.views.history_structure_navigation.commit_module_facet = {
      status: "available",
      value: false,
    };

    const { container } = render(<RepoShowcase bundle={bundle} />);
    await user.click(container.querySelector('[data-view-id="history_structure_navigation"]')!);

    const commits = container.querySelector<HTMLElement>("[data-history-commits]")!;
    expect(within(commits).getAllByText("0").length).toBeGreaterThan(0);
    expect(within(commits).queryByText(bundle.capability.labels.unavailable)).not.toBeInTheDocument();
    expect(within(container.querySelector<HTMLElement>("[data-history-capabilities]")!).getByText("false")).toBeVisible();
  });

  it("reads chart and table labels from capability rather than UI constants", async () => {
    const bundle = structuredClone(golden());
    const user = userEvent.setup();
    bundle.capability.views.hotspot_quadrant.label = "Contract-owned risk field";
    bundle.capability.labels.metrics.fan_in = "Contract-owned inbound degree";
    bundle.capability.labels.metrics.change_frequency = "Contract-owned revisions";
    const quadrant = bundle.aggregates.hotspot_quadrant.data.items[0].quadrant;
    bundle.capability.labels.hotspot_quadrants[quadrant] = "Contract-owned quadrant";
    bundle.capability.labels.metrics.p50 = "Contract-owned median";
    bundle.aggregates.cadence_timeline.pull_request_lead_time_hours = {
      status: "available",
      value: { p50: 4, p90: 9, p95: 12 },
    };

    const { container } = render(<RepoShowcase bundle={bundle} />);
    await user.click(screen.getByRole("button", { name: "Contract-owned risk field" }));

    expect(screen.getByRole("heading", { name: "Contract-owned risk field" })).toBeVisible();
    expect(screen.getAllByText("Contract-owned inbound degree").length).toBeGreaterThan(0);
    expect(screen.getAllByText("Contract-owned revisions").length).toBeGreaterThan(0);
    expect(screen.getAllByText("Contract-owned quadrant").length).toBeGreaterThan(0);

    await user.click(container.querySelector('[data-view-id="cadence_timeline"]')!);
    expect(screen.getByText(/Contract-owned median 4\.0/)).toBeVisible();
  });

  it("surfaces a section's own truncation disclosure", () => {
    const bundle = structuredClone(golden());
    bundle.graph.modules.truncated = true;
    bundle.graph.modules.disclosure = { status: "truncated", reason: "fixture node budget" };
    bundle.graph.modules.next_cursor = "opaque-cursor";

    render(<RepoShowcase bundle={bundle} />);

    expect(screen.getByText(/fixture node budget/i)).toBeVisible();
    expect(screen.getAllByText((content) => content.includes(bundle.capability.labels.truncated)).length).toBeGreaterThan(0);
  });

  it("keeps repository-wide ownership visible when the module join is unavailable", async () => {
    const bundle = structuredClone(golden());
    const user = userEvent.setup();
    bundle.capability.views.ownership.status = "unavailable";
    bundle.capability.views.ownership.unavailable_reason = "module join was not complete";
    bundle.aggregates.ownership.modules = {
      ...bundle.aggregates.ownership.modules,
      items: [],
      total_count: { status: "unavailable", reason: "module join was not complete" },
      next_cursor: null,
      truncated: false,
      disclosure: { status: "unavailable", reason: "module join was not complete" },
    };
    bundle.aggregates.ownership.repository_author_concentration = { status: "available", value: 0.42 };
    bundle.aggregates.ownership.repository_bus_factor = { status: "available", value: 3 };

    const { container } = render(<RepoShowcase bundle={bundle} />);
    await user.click(container.querySelector('[data-view-id="ownership"]')!);

    expect(screen.getByText("42%")).toBeVisible();
    expect(screen.getAllByText("3").length).toBeGreaterThan(0);
    expect(screen.getAllByText(/module join was not complete/i).length).toBeGreaterThan(0);
  });

  it("renders every cadence series with its own availability and bound", async () => {
    const bundle = golden();
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);
    await user.click(container.querySelector('[data-view-id="cadence_timeline"]')!);

    expect(container.querySelector('[data-cadence-series="commits"]')).toHaveAttribute("data-series-status", "complete");
    for (const id of ["issues_opened", "issues_closed", "pull_requests_opened", "pull_requests_merged"]) {
      const series = container.querySelector<HTMLElement>(`[data-cadence-series="${id}"]`)!;
      expect(series).toHaveAttribute("data-series-status", "unavailable");
      expect(within(series).getAllByText(bundle.capability.labels.unavailable).length).toBeGreaterThan(0);
    }
  });

  it("caps large browser tables and discloses the local slice", async () => {
    const bundle = golden();
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);
    await user.click(container.querySelector('[data-view-id="hotspot_quadrant"]')!);

    expect(container.querySelectorAll(".repo-view-panel tbody tr")).toHaveLength(200);
    expect(Array.from(container.querySelectorAll(".repo-view-panel .repo-bounded.truncated")).some((node) => node.textContent?.includes(bundle.capability.labels.truncated))).toBe(true);
  });
});
