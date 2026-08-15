import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { RepoShowcase } from "@/components/showcase/repo-showcase";
import { parseRepoBundle, type RepoBundle } from "@/lib/repo-bundle";

const goldenPath = resolve(process.cwd(), "../../docs/schemas/examples/khive-repo-v1-khive.json");
const showcaseSourcePath = resolve(process.cwd(), "src/components/showcase/repo-showcase.tsx");
const studioSourcePath = resolve(process.cwd(), "src/components/studio.tsx");

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

  it("keeps legacy state markup out of Studio and repository showcase surfaces", () => {
    const showcaseSource = readFileSync(showcaseSourcePath, "utf8");
    const studioSource = readFileSync(studioSourcePath, "utf8");

    expect(showcaseSource).not.toMatch(/<div className="repo-empty/);
    expect(showcaseSource).not.toMatch(/<div className={`repo-bounded/);
    expect(showcaseSource).not.toMatch(/return <em className="repo-inline-state"/);
    expect(studioSource).not.toMatch(/<div className="page-notice"/);
  });

  it("uses the shared unavailable state and exposes the capability reason", async () => {
    const bundle = structuredClone(golden());
    const user = userEvent.setup();
    const view = bundle.capability.views.scorecard;
    view.status = "unavailable";
    view.unavailable_reason = "scorecard evidence was outside this export";

    const { container } = render(<RepoShowcase bundle={bundle} />);
    await user.click(container.querySelector('[data-view-id="scorecard"]')!);

    const unavailable = container.querySelector<HTMLElement>('[data-state="unavailable"]');
    expect(unavailable).toBeVisible();
    expect(unavailable).toHaveTextContent(view.unavailable_reason);
  });

  it("uses the shared ontology legend and marks exporter-derived edges geometrically", () => {
    const { container } = render(<RepoShowcase bundle={golden()} />);

    expect(screen.getByLabelText("Ontology legend")).toHaveTextContent(/Concept.*Project.*Contains.*Derived/i);
    expect(container.querySelector('line[data-edge-origin="derived"]')).toHaveAttribute("marker-end", "url(#showcase-ontology-arrow)");
    expect(container.querySelector(".ontology-direction-glyph")?.getAttribute("transform")).toMatch(/^rotate\(/);
    expect(container.querySelector("polygon.ontology-derived-glyph")).toBeInTheDocument();
  });

  it("lays out the structure graph from the shared seeded layout, independent of input order", () => {
    const bundle = golden();
    const reversed = structuredClone(bundle);
    reversed.graph.packages = {
      ...reversed.graph.packages,
      items: [...reversed.graph.packages.items].reverse(),
    };
    reversed.graph.modules = {
      ...reversed.graph.modules,
      items: [...reversed.graph.modules.items].reverse(),
    };

    function nodePositions(container: HTMLElement): Record<string, { left: string; top: string }> {
      const positions: Record<string, { left: string; top: string }> = {};
      for (const node of container.querySelectorAll<HTMLElement>(".repo-graph-node[data-node-id]")) {
        const id = node.getAttribute("data-node-id")!;
        positions[id] = { left: node.style.left, top: node.style.top };
      }
      return positions;
    }

    const forward = render(<RepoShowcase bundle={bundle} />);
    const forwardPositions = nodePositions(forward.container);
    expect(Object.keys(forwardPositions).length).toBeGreaterThan(0);
    forward.unmount();

    const backward = render(<RepoShowcase bundle={reversed} />);
    const backwardPositions = nodePositions(backward.container);
    backward.unmount();

    expect(backwardPositions).toEqual(forwardPositions);
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
    const empty = commits.querySelector<HTMLElement>('[data-state="empty"]');
    expect(empty).toBeVisible();
    expect(empty?.querySelectorAll("button")).toHaveLength(1);
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

    const { container } = render(<RepoShowcase bundle={bundle} />);

    // Multiple `[data-state="truncated"]` disclosures can coexist: the browser-side
    // local display slice (bound = shown, always ambient once a list exceeds the UI
    // row cap) is a separate concern from the bundle's own page-bound disclosure.
    // Target the modules page's own disclosure by its distinguishing reason text.
    const state = Array.from(container.querySelectorAll<HTMLElement>('[data-state="truncated"]'))
      .find((node) => node.textContent?.includes("fixture node budget"));
    expect(state).toBeVisible();
    expect(state).toHaveAttribute("data-bound", String(bundle.graph.modules.bound.max_items));
    if (bundle.graph.modules.total_count.status === "available") {
      expect(state).toHaveAttribute("data-known-total", String(bundle.graph.modules.total_count.value));
    }
    expect(state).toHaveTextContent(/fixture node budget/i);
  });

  it.each([
    ["truncated", { truncated: true, next_cursor: null, disclosure: "truncated" }],
    ["next cursor", { truncated: false, next_cursor: "next-page", disclosure: "complete" }],
  ] as const)("does not mislabel a zero-item %s repository page as known-empty", async (_name, incomplete) => {
    const bundle = structuredClone(golden());
    const user = userEvent.setup();
    const cycles = bundle.aggregates.dependency_topology.cycles;
    cycles.items = [];
    cycles.truncated = incomplete.truncated;
    cycles.next_cursor = incomplete.next_cursor;
    cycles.total_count = { status: "available", value: 3 };
    cycles.disclosure = { status: incomplete.disclosure, reason: "fixture incomplete cycle page" };

    const { container } = render(<RepoShowcase bundle={bundle} />);
    await user.click(container.querySelector('[data-view-id="dependency_topology"]')!);

    const card = screen.getByRole("heading", { name: bundle.capability.labels.metrics.cycle_count }).closest("section")!;
    expect(card.querySelector('[data-state="empty"]')).not.toBeInTheDocument();
    const state = card.querySelector<HTMLElement>('[data-state="truncated"]');
    expect(state).toBeVisible();
    expect(state).toHaveAttribute("data-shown", "0");
    expect(state).toHaveAttribute("data-bound", String(cycles.bound.max_items));
  });

  it.each([
    ["nonempty", false],
    ["empty", true],
  ] as const)("treats an omitted cursor on a complete %s repository page as complete", async (_name, empty) => {
    const bundle = structuredClone(golden());
    const user = userEvent.setup();
    const cycles = bundle.aggregates.dependency_topology.cycles;
    cycles.items = empty ? [] : cycles.items.slice(0, 1);
    cycles.truncated = false;
    cycles.disclosure = { status: "complete", reason: null };
    Reflect.deleteProperty(cycles, "next_cursor");

    const { container } = render(<RepoShowcase bundle={bundle} />);
    await user.click(container.querySelector('[data-view-id="dependency_topology"]')!);

    const card = screen.getByRole("heading", { name: bundle.capability.labels.metrics.cycle_count }).closest("section")!;
    expect(card.querySelector('[data-state="truncated"]')).not.toBeInTheDocument();
    if (empty) expect(card.querySelector('[data-state="empty"]')).toBeVisible();
    else expect(card.querySelector('[data-state="empty"]')).not.toBeInTheDocument();
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
    expect(Array.from(container.querySelectorAll(".repo-view-panel .repo-bounded.truncated")).some((node) => node.textContent?.toLocaleLowerCase().includes(bundle.capability.labels.truncated.toLocaleLowerCase()))).toBe(true);
  });

  it("settles the structure graph without collapsing nodes onto a handful of shared coordinates", () => {
    const { container } = render(<RepoShowcase bundle={golden()} />);
    const nodes = Array.from(container.querySelectorAll<HTMLElement>(".repo-graph-node[data-node-id]"));
    expect(nodes).toHaveLength(51);

    const coordinateCounts = new Map<string, number>();
    for (const node of nodes) {
      const key = `${node.style.left}|${node.style.top}`;
      coordinateCounts.set(key, (coordinateCounts.get(key) ?? 0) + 1);
    }

    const overcrowded = Array.from(coordinateCounts.entries()).filter(([, count]) => count > 2);
    expect(overcrowded).toEqual([]);
  });

  it("keeps every structure-graph card footprint inside a 300px mobile stage", () => {
    const stageWidth = 300;
    const { container } = render(<RepoShowcase bundle={golden()} />);
    const nodes = Array.from(container.querySelectorAll<HTMLElement>(".repo-graph-node[data-node-id]"));
    expect(nodes).toHaveLength(51);

    for (const node of nodes) {
      const leftPercent = Number.parseFloat(node.style.left);
      const widthPx = Number.parseFloat(node.style.width);
      expect(Number.isNaN(leftPercent)).toBe(false);
      expect(Number.isNaN(widthPx)).toBe(false);
      const center = leftPercent / 100 * stageWidth;
      const halfWidth = widthPx / 2;
      expect(center - halfWidth).toBeGreaterThanOrEqual(0);
      expect(center + halfWidth).toBeLessThanOrEqual(stageWidth);
    }
  });
});
