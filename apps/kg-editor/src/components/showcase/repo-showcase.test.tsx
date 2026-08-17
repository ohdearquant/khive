import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { RepoShowcase } from "@/components/showcase/repo-showcase";
import { parseRepoBundle, type RepoBundle } from "@/lib/repo-bundle";
import { repositoryLocationUrl } from "@/lib/repository-location";

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
  beforeEach(() => {
    window.history.replaceState(null, "", "/");
  });

  it("restores a shareable module and analysis location from a direct deep link", async () => {
    const bundle = golden();
    const pool = bundle.graph.modules.items.find((module) =>
      module.source_path.endsWith("khive-db/src/pool.rs")
    )!;
    const direct = repositoryLocationUrl(new URL(window.location.href), {
      repository: bundle.meta.repository.canonical_url,
      snapshotSha: bundle.meta.snapshot.head_sha,
      modulePath: pool.source_path,
      view: "dependency_topology",
    });
    window.history.replaceState(null, "", direct);
    const { container } = render(<RepoShowcase bundle={bundle} />);

    const inspector = within(
      container.querySelector<HTMLElement>("[data-repository-triage]")!,
    ).getByRole("complementary", { name: "Module evidence" });
    await waitFor(() =>
      expect(within(inspector).getByRole("heading", { level: 3 })).toHaveTextContent(
        pool.source_path,
      )
    );
    expect(
      screen.getByRole("button", {
        name: bundle.capability.views.dependency_topology.label,
      }),
    ).toHaveAttribute("aria-current", "page");
    const breadcrumb = within(inspector).getByRole("navigation", {
      name: "Investigation location",
    });
    expect(breadcrumb).toHaveTextContent(`${bundle.meta.repository.owner}/${bundle.meta.repository.name}`);
    expect(breadcrumb).toHaveTextContent(pool.source_path);
  });

  it("navigates to a module found via search and updates the URL", async () => {
    const bundle = golden();
    const writer = bundle.graph.modules.items.find((module) =>
      module.source_path.endsWith("khive-db/src/writer_task.rs")
    )!;
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);
    const inspector = within(
      container.querySelector<HTMLElement>("[data-repository-triage]")!,
    ).getByRole("complementary", { name: "Module evidence" });
    const pushState = vi.spyOn(window.history, "pushState");
    pushState.mockClear();

    const search = screen.getByRole("searchbox", { name: "Find a module or path" });
    await user.type(search, "writer_task.rs");
    await user.click(within(screen.getByLabelText("Module search results"))
      .getByRole("button", { name: `Inspect ${writer.source_path}` }));
    await waitFor(() =>
      expect(within(inspector).getByRole("heading", { level: 3 })).toHaveTextContent(
        writer.source_path,
      )
    );
    expect(new URL(window.location.href).searchParams.get("module")).toBe(
      writer.source_path,
    );
    expect(pushState).toHaveBeenCalledTimes(1);
  });

  it("switches the active analysis view via the location controller, ignoring a duplicate click", async () => {
    const bundle = golden();
    const writer = bundle.graph.modules.items.find((module) =>
      module.source_path.endsWith("khive-db/src/writer_task.rs")
    )!;
    const direct = repositoryLocationUrl(new URL(window.location.href), {
      repository: bundle.meta.repository.canonical_url,
      snapshotSha: bundle.meta.snapshot.head_sha,
      modulePath: writer.source_path,
      view: "dependency_topology",
    });
    window.history.replaceState(null, "", direct);
    const user = userEvent.setup();
    render(<RepoShowcase bundle={bundle} />);
    await waitFor(() =>
      expect(
        screen.getByRole("button", {
          name: bundle.capability.views.dependency_topology.label,
        }),
      ).toHaveAttribute("aria-current", "page")
    );
    const pushState = vi.spyOn(window.history, "pushState");
    pushState.mockClear();

    const hiddenCoupling = screen.getByRole("button", {
      name: bundle.capability.views.hidden_coupling.label,
    });
    await user.click(hiddenCoupling);
    expect(new URL(window.location.href).searchParams.get("view")).toBe(
      "hidden_coupling",
    );
    expect(pushState).toHaveBeenCalledTimes(1);
    await user.click(hiddenCoupling);
    expect(pushState).toHaveBeenCalledTimes(1);
  });

  it("restores investigation state on browser back navigation without pushing new history", async () => {
    const bundle = golden();
    const pool = bundle.graph.modules.items.find((module) =>
      module.source_path.endsWith("khive-db/src/pool.rs")
    )!;
    const writer = bundle.graph.modules.items.find((module) =>
      module.source_path.endsWith("khive-db/src/writer_task.rs")
    )!;
    const start = repositoryLocationUrl(new URL(window.location.href), {
      repository: bundle.meta.repository.canonical_url,
      snapshotSha: bundle.meta.snapshot.head_sha,
      modulePath: writer.source_path,
      view: "hidden_coupling",
    });
    window.history.replaceState(null, "", start);
    const { container } = render(<RepoShowcase bundle={bundle} />);
    const inspector = within(
      container.querySelector<HTMLElement>("[data-repository-triage]")!,
    ).getByRole("complementary", { name: "Module evidence" });
    await waitFor(() =>
      expect(within(inspector).getByRole("heading", { level: 3 })).toHaveTextContent(
        writer.source_path,
      )
    );
    const pushState = vi.spyOn(window.history, "pushState");
    pushState.mockClear();

    const direct = repositoryLocationUrl(new URL(window.location.href), {
      repository: bundle.meta.repository.canonical_url,
      snapshotSha: bundle.meta.snapshot.head_sha,
      modulePath: pool.source_path,
      view: "dependency_topology",
    });
    window.history.replaceState(null, "", direct);
    window.dispatchEvent(new PopStateEvent("popstate"));
    await waitFor(() =>
      expect(within(inspector).getByRole("heading", { level: 3 })).toHaveTextContent(
        pool.source_path,
      )
    );
    expect(
      screen.getByRole("button", {
        name: bundle.capability.views.dependency_topology.label,
      }),
    ).toHaveAttribute("aria-current", "page");
    expect(
      screen.getByRole("status", { name: "Investigation navigation" }),
    ).toHaveTextContent(
      `Restored ${bundle.capability.views.dependency_topology.label} for ${pool.source_path}.`,
    );
    expect(pushState).not.toHaveBeenCalled();
  });

  it("keeps stale and missing deep-link evidence explicit and recoverable", async () => {
    const bundle = golden();
    const missingPath = "crates/not-captured/src/missing.rs";
    const direct = repositoryLocationUrl(new URL(window.location.href), {
      repository: bundle.meta.repository.canonical_url,
      snapshotSha: "0000000000000000000000000000000000000000",
      modulePath: missingPath,
      view: "scorecard",
    });
    window.history.replaceState(null, "", direct);
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);

    expect(await screen.findByRole("status", { name: /investigation link/i }))
      .toHaveTextContent(/requested snapshot.*is not loaded/i);
    const inspector = container.querySelector<HTMLElement>("[data-module-inspector]")!;
    expect(within(inspector).getByText(new RegExp(missingPath))).toBeVisible();
    const recovery = within(inspector).getByRole("button", {
      name: /open recommended module/i,
    });
    await user.click(recovery);

    await waitFor(() =>
      expect(new URL(window.location.href).searchParams.get("at")).toBe(
        bundle.meta.snapshot.head_sha,
      )
    );
    await waitFor(() => expect(inspector).toHaveFocus());
    expect(new URL(window.location.href).searchParams.get("module")).not.toBe(
      missingPath,
    );
  });

  it("copies the normalized investigation link with visible feedback", async () => {
    const user = userEvent.setup();
    const writeText = vi.spyOn(navigator.clipboard, "writeText")
      .mockResolvedValue(undefined);
    writeText.mockClear();
    render(<RepoShowcase bundle={golden()} />);

    await waitFor(() =>
      expect(new URL(window.location.href).searchParams.get("at")).not.toBeNull()
    );
    await user.click(
      screen.getByRole("button", { name: "Copy investigation link" }),
    );

    expect(writeText).toHaveBeenCalledWith(window.location.href);
    expect(screen.getByText("Investigation link copied.")).toHaveAttribute(
      "role",
      "status",
    );
  });

  it("preserves stale-link evidence when clipboard access fails", async () => {
    const bundle = golden();
    const staleSha = "0000000000000000000000000000000000000000";
    const direct = repositoryLocationUrl(new URL(window.location.href), {
      repository: bundle.meta.repository.canonical_url,
      snapshotSha: staleSha,
      modulePath: bundle.graph.modules.items[0].source_path,
      view: "scorecard",
    });
    window.history.replaceState(null, "", direct);
    const writeText = vi.spyOn(navigator.clipboard, "writeText")
      .mockRejectedValue(new Error("permission denied"));
    writeText.mockClear();
    const user = userEvent.setup();
    render(<RepoShowcase bundle={bundle} />);

    const notice = await screen.findByRole("status", {
      name: "Investigation link status",
    });
    await user.click(
      screen.getByRole("button", { name: "Copy investigation link" }),
    );

    expect(writeText).toHaveBeenCalledOnce();
    expect(new URL(window.location.href).searchParams.get("at")).toBe(staleSha);
    expect(notice).toBeVisible();
    expect(screen.getByText("Investigation link could not be copied."))
      .toHaveAttribute("role", "status");
    await user.click(within(notice).getByRole("button", {
      name: "Use current snapshot",
    }));
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Copy investigation link" }))
        .toHaveFocus()
    );
  });

  it("does not roll navigation back when clipboard completion is delayed", async () => {
    const bundle = golden();
    let finishCopy: (() => void) | undefined;
    const writeText = vi.spyOn(navigator.clipboard, "writeText")
      .mockImplementation(() => new Promise<void>((resolve) => {
        finishCopy = resolve;
      }));
    writeText.mockClear();
    const user = userEvent.setup();
    render(<RepoShowcase bundle={bundle} />);
    await waitFor(() =>
      expect(new URL(window.location.href).searchParams.get("view"))
        .toBe("structure_graph")
    );

    await user.click(
      screen.getByRole("button", { name: "Copy investigation link" }),
    );
    await waitFor(() => expect(writeText).toHaveBeenCalledOnce());
    await user.click(screen.getByRole("button", {
      name: bundle.capability.views.hidden_coupling.label,
    }));
    expect(new URL(window.location.href).searchParams.get("view"))
      .toBe("hidden_coupling");

    finishCopy?.();
    expect(await screen.findByText("Investigation link copied."))
      .toBeVisible();
    expect(new URL(window.location.href).searchParams.get("view"))
      .toBe("hidden_coupling");
  });

  it("labels generic repaired locations separately from stale snapshots", async () => {
    const bundle = golden();
    window.history.replaceState(
      null,
      "",
      `/?repo=${encodeURIComponent(bundle.meta.repository.canonical_url)}&at=${bundle.meta.snapshot.head_sha}&view=not-a-view`,
    );
    const user = userEvent.setup();
    render(<RepoShowcase bundle={bundle} />);

    const notice = await screen.findByRole("status", {
      name: "Investigation link status",
    });
    expect(notice).toHaveTextContent("Investigation link was repaired");
    expect(within(notice).queryByRole("button", { name: "Use current snapshot" }))
      .not.toBeInTheDocument();
    await user.click(within(notice).getByRole("button", { name: "Dismiss" }));
    expect(notice).not.toBeInTheDocument();
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Copy investigation link" }))
        .toHaveFocus()
    );
  });

  it("does not offer a no-op module recovery for a snapshot with no modules", async () => {
    const bundle = structuredClone(golden());
    bundle.graph.modules.items = [];
    window.history.replaceState(
      null,
      "",
      `/?repo=${encodeURIComponent(bundle.meta.repository.canonical_url)}&at=${bundle.meta.snapshot.head_sha}&module=crates%2Fmissing.rs&view=scorecard`,
    );
    const { container } = render(<RepoShowcase bundle={bundle} />);

    const inspector = container.querySelector<HTMLElement>(
      "[data-module-inspector]",
    )!;
    const unavailable = await within(inspector).findByRole("status");
    expect(unavailable).toHaveAttribute("data-state", "unavailable");
    expect(unavailable).toHaveTextContent(/no captured modules/i);
    expect(unavailable.querySelector("button")).toBeNull();
  });

  it("answers repository triage questions before exposing the raw analysis views", async () => {
    const bundle = golden();
    const user = userEvent.setup();

    const { container } = render(<RepoShowcase bundle={bundle} />);

    const triage = screen.getByRole("region", { name: "Repository triage" });
    expect(within(triage).getByRole("heading", { name: "What deserves attention?" })).toBeVisible();
    expect(container.querySelector('[data-repository-metric="modules"]')).toHaveTextContent(`${bundle.graph.modules.items.length}`);
    expect(container.querySelector('[data-repository-metric="modules"]')).toHaveTextContent(/complete/i);
    expect(container.querySelector('[data-repository-metric="commits"]')).toHaveTextContent(`${bundle.graph.commits.items.length}`);
    expect(within(triage).getByText("Observed", { selector: "span" })).toBeVisible();
    expect(within(triage).getAllByText("Candidate", { selector: "span" }).length).toBeGreaterThan(0);
    expect(container.querySelector('[data-signal-kind="hidden_coupling"]')).toHaveTextContent(/truncated/i);

    const firstStart = within(triage).getAllByRole("button", { name: /inspect .*\.rs/i })[0];
    await user.click(firstStart);
    const inspector = within(triage).getByRole("complementary", { name: "Module evidence" });
    expect(within(inspector).getByText("Analysis window")).toBeVisible();
    expect(within(inspector).getByRole("heading", { name: bundle.capability.labels.metrics.commits })).toBeVisible();

    await user.type(within(triage).getByRole("searchbox", { name: "Find a module or path" }), "pool.rs");
    const poolResult = within(triage).getByRole("button", { name: /inspect .*pool\.rs/i });
    await user.click(poolResult);
    expect(within(inspector).getByRole("heading", { level: 3 }).textContent).toContain("pool.rs");

    await user.click(within(triage).getAllByRole("button", { name: /open full analysis/i })[0]);
    expect(screen.getByRole("button", { name: bundle.capability.views.hotspot_quadrant.label })).toHaveAttribute("aria-current", "page");
    expect(screen.getByRole("navigation", { name: bundle.capability.labels.product })).toBeVisible();
  });

  it("keeps module search bounded and gives an empty result one recovery action", async () => {
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={golden()} />);
    const triage = container.querySelector<HTMLElement>("[data-repository-triage]")!;

    await user.type(within(triage).getByRole("searchbox", { name: "Find a module or path" }), "definitely-not-a-module");

    const empty = triage.querySelector<HTMLElement>('[data-state="empty"]')!;
    expect(empty).toBeVisible();
    expect(empty.querySelectorAll("button")).toHaveLength(1);
    expect(within(triage).getAllByRole("button", { name: "Clear search" })).toHaveLength(1);
    await user.click(within(empty).getByRole("button", { name: "Clear search" }));
    expect(within(triage).getByRole("searchbox", { name: "Find a module or path" })).toHaveValue("");
  });

  it("moves focus and scroll position to the analysis selected from triage", async () => {
    const user = userEvent.setup();
    const scrollIntoView = vi.fn();
    Object.defineProperty(HTMLElement.prototype, "scrollIntoView", {
      configurable: true,
      value: scrollIntoView,
    });
    const { container } = render(<RepoShowcase bundle={golden()} />);
    const triage = container.querySelector<HTMLElement>("[data-repository-triage]")!;

    await user.click(within(triage).getAllByRole("button", { name: /open full analysis/i })[0]);

    const dashboard = container.querySelector<HTMLElement>("[data-repository-dashboard]")!;
    await waitFor(() => expect(dashboard).toHaveFocus());
    expect(scrollIntoView).toHaveBeenCalledOnce();
  });

  it("distinguishes unavailable recommendation analyses from measured empty", () => {
    const bundle = structuredClone(golden());
    bundle.aggregates.api_surface.meta.status = "unavailable";
    bundle.aggregates.api_surface.meta.unavailable_reason = "API ranking was not produced";
    for (const analysis of [bundle.aggregates.hotspot_quadrant, bundle.aggregates.dependency_topology, bundle.aggregates.hidden_coupling, bundle.aggregates.ownership]) {
      analysis.meta.status = "unavailable";
      analysis.meta.unavailable_reason = "attention analysis was not produced";
    }

    const { container } = render(<RepoShowcase bundle={bundle} />);
    const triage = container.querySelector<HTMLElement>("[data-repository-triage]")!;

    expect(within(triage).getByText("API ranking was not produced")).toBeVisible();
    expect(within(triage).getAllByText(/attention analysis was not produced/i).length).toBeGreaterThan(0);
    expect(triage.querySelectorAll('[data-state="unavailable"]').length).toBeGreaterThanOrEqual(2);
    expect(triage).not.toHaveTextContent(/No captured module has a non-zero/i);
  });

  it("navigates inspector relationships and discloses cycles and history bounds", async () => {
    const bundle = golden();
    const cycle = bundle.aggregates.dependency_topology.cycles.items.find((candidate) => candidate.module_ids.length > 1)!;
    const selected = bundle.graph.modules.items.find((module) => module.id === cycle.module_ids[0])!;
    const peer = bundle.graph.modules.items.find((module) => module.id === cycle.module_ids[1])!;
    const history = bundle.graph.history_navigation.by_module.items.find((row) => row.module_id === selected.id)!.commits;
    const scrollIntoView = vi.fn();
    Object.defineProperty(HTMLElement.prototype, "scrollIntoView", { configurable: true, value: scrollIntoView });
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);
    const triage = container.querySelector<HTMLElement>("[data-repository-triage]")!;

    await user.type(within(triage).getByRole("searchbox", { name: "Find a module or path" }), selected.source_path);
    await user.click(within(triage).getByRole("button", { name: `Inspect ${selected.source_path}` }));

    const inspector = within(triage).getByRole("complementary", { name: "Module evidence" });
    await waitFor(() => expect(inspector).toHaveFocus());
    expect(scrollIntoView).toHaveBeenCalled();
    expect(within(inspector).getByText(cycle.id)).toBeVisible();
    expect(within(inspector).getByText("Not classified in this snapshot")).toBeVisible();
    const historyMetric = inspector.querySelector<HTMLElement>('[data-inspector-metric="commits"]')!;
    expect(historyMetric).toHaveTextContent(String(history.total_count.status === "available" ? history.total_count.value : history.items.length));
    expect(inspector).toHaveTextContent(/inspector sampled/i);

    const cycleSection = within(inspector).getByText(cycle.id).closest("section")!;
    await user.click(within(cycleSection).getByRole("button", { name: peer.source_path }));
    expect(within(inspector).getByRole("heading", { level: 3 })).toHaveTextContent(peer.source_path);
  });

  it("renders strongly connected components as membership, not a directed path", async () => {
    const bundle = golden();
    const cycle = bundle.aggregates.dependency_topology.cycles.items[0];
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);

    await user.click(screen.getByRole("button", { name: bundle.capability.views.dependency_topology.label }));

    const cycleRow = screen.getByText(cycle.id).closest(".repo-list-row")!;
    expect(cycleRow).toHaveTextContent("SCC members:");
    expect(cycleRow).not.toHaveTextContent("→");
    expect(container.querySelector("[data-repository-dashboard]")).toBeVisible();
  });

  it("renders missing module history as unavailable rather than zero", () => {
    const bundle = structuredClone(golden());
    const target = [...bundle.aggregates.api_surface.data.items].sort((left, right) => right.dependent_count - left.dependent_count)[0];
    bundle.graph.history_navigation.by_module.items = bundle.graph.history_navigation.by_module.items.filter((row) => row.module_id !== target.module_id);

    const { container } = render(<RepoShowcase bundle={bundle} />);
    const inspector = container.querySelector<HTMLElement>("[data-module-inspector]")!;
    const commitMetric = inspector.querySelector<HTMLElement>('[data-inspector-metric="commits"]')!;

    expect(commitMetric).toHaveTextContent(bundle.capability.labels.unavailable);
    expect(commitMetric).not.toHaveTextContent(/^0$/);
    expect(inspector).toHaveTextContent("No module history-navigation row was captured.");
  });

  it("renders unavailable topology metrics as unavailable rather than zero", () => {
    const bundle = structuredClone(golden());
    bundle.aggregates.dependency_topology.meta.status = "unavailable";
    bundle.aggregates.dependency_topology.meta.unavailable_reason =
      "topology analysis was not produced";
    bundle.graph.structure_edges.disclosure.status = "unavailable";
    bundle.graph.structure_edges.disclosure.reason =
      "structure edges were not produced";

    const { container } = render(<RepoShowcase bundle={bundle} />);
    const inspector = container.querySelector<HTMLElement>(
      "[data-module-inspector]",
    )!;

    for (const metric of ["fan-in", "fan-out"]) {
      const row = inspector.querySelector<HTMLElement>(
        `[data-inspector-metric="${metric}"]`,
      )!;
      expect(row).toHaveTextContent(bundle.capability.labels.unavailable);
      expect(row).not.toHaveTextContent(/^0$/);
    }
    expect(inspector).toHaveTextContent("topology analysis was not produced");
  });

  it("shows unavailable coupling evidence instead of a measured-empty claim", () => {
    const bundle = structuredClone(golden());
    bundle.aggregates.hidden_coupling.meta.status = "unavailable";
    bundle.aggregates.hidden_coupling.meta.unavailable_reason = "coupling analysis was not produced";

    const { container } = render(<RepoShowcase bundle={bundle} />);
    const inspector = container.querySelector<HTMLElement>("[data-module-inspector]")!;

    expect(inspector).toHaveTextContent("coupling analysis was not produced");
    expect(inspector).not.toHaveTextContent(/No pair for this module appears/i);
  });

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
    const moduleNode = bundle.graph.modules.items.find((module) =>
      module.id === first.module_id
    )!;
    window.history.replaceState(
      null,
      "",
      repositoryLocationUrl(new URL(window.location.href), {
        repository: bundle.meta.repository.canonical_url,
        snapshotSha: bundle.meta.snapshot.head_sha,
        modulePath: moduleNode.source_path,
        view: "history_structure_navigation",
      }),
    );

    const { container } = render(<RepoShowcase bundle={bundle} />);
    await waitFor(() =>
      expect(container.querySelector('[data-view-id="history_structure_navigation"]'))
        .toHaveAttribute("aria-current", "page")
    );

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
    bundle.capability.views.dependency_topology.label = "Contract-owned topology";
    bundle.capability.views.hidden_coupling.label = "Contract-owned coupling";
    bundle.capability.views.api_surface.label = "Contract-owned API surface";
    bundle.capability.labels.node_types.module = "Contract-owned component";
    bundle.capability.labels.metrics.package_count = "Contract-owned package count";
    bundle.capability.labels.metrics.module_count = "Contract-owned module count";
    bundle.capability.labels.metrics.commits = "Contract-owned commits";
    bundle.capability.labels.metrics.cycle_count = "Contract-owned cycles";
    bundle.capability.labels.metrics.fan_in = "Contract-owned inbound degree";
    bundle.capability.labels.metrics.fan_out = "Contract-owned outbound degree";
    bundle.capability.labels.metrics.bus_factor = "Contract-owned bus factor";
    bundle.capability.labels.metrics.dependent_count = "Contract-owned dependent count";
    bundle.capability.labels.metrics.cochange_count = "Contract-owned co-change count";
    bundle.capability.labels.metrics.support = "Contract-owned support";
    bundle.capability.labels.metrics.change_frequency = "Contract-owned revisions";
    for (const quadrant of Object.keys(bundle.capability.labels.hotspot_quadrants) as Array<keyof typeof bundle.capability.labels.hotspot_quadrants>) {
      bundle.capability.labels.hotspot_quadrants[quadrant] = "Contract-owned quadrant";
    }
    bundle.capability.labels.metrics.p50 = "Contract-owned median";
    bundle.aggregates.cadence_timeline.pull_request_lead_time_hours = {
      status: "available",
      value: { p50: 4, p90: 9, p95: 12 },
    };

    const { container } = render(<RepoShowcase bundle={bundle} />);
    const triage = container.querySelector<HTMLElement>("[data-repository-triage]")!;
    for (const label of [
      "Contract-owned package count",
      "Contract-owned module count",
      "Contract-owned commits",
      "Contract-owned cycles",
      "Contract-owned inbound degree",
      "Contract-owned outbound degree",
      "Contract-owned bus factor",
      "Contract-owned topology",
      "Contract-owned coupling",
      "Contract-owned component evidence",
    ]) {
      expect(within(triage).getAllByText(label).length).toBeGreaterThan(0);
    }
    expect(triage).toHaveTextContent("Contract-owned API surface");
    expect(triage).toHaveTextContent("Contract-owned dependent count");
    expect(triage).toHaveTextContent("Contract-owned quadrant");
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

    const state = container.querySelector<HTMLElement>(`.repo-view-panel [data-state="truncated"][data-bound="${bundle.graph.modules.bound.max_items}"]`);
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
    const localSlice = container.querySelector<HTMLElement>(`.repo-view-panel [data-state="truncated"][data-shown="200"][data-known-total="${bundle.aggregates.hotspot_quadrant.data.items.length}"]`);
    expect(localSlice).toBeVisible();
    expect(localSlice).toHaveTextContent(/truncated/i);
  });

  it("routes palette module, view, and copy commands through the shared location controller", async () => {
    const bundle = golden();
    const writer = bundle.graph.modules.items.find((module) =>
      module.source_path.endsWith("khive-db/src/writer_task.rs")
    )!;
    const user = userEvent.setup();
    const writeText = vi.spyOn(navigator.clipboard, "writeText")
      .mockResolvedValue(undefined);
    writeText.mockClear();
    const { container } = render(<RepoShowcase bundle={bundle} />);
    await waitFor(() =>
      expect(new URL(window.location.href).searchParams.get("view"))
        .toBe("structure_graph")
    );
    const pushState = vi.spyOn(window.history, "pushState");
    pushState.mockClear();

    await user.keyboard("{Meta>}k{/Meta}");
    await user.type(
      screen.getByRole("combobox", { name: "Search repository commands" }),
      writer.source_path,
    );
    await user.keyboard("{Enter}");
    const inspector = container.querySelector<HTMLElement>(
      "[data-module-inspector]",
    )!;
    await waitFor(() =>
      expect(within(inspector).getByRole("heading", { level: 3 }))
        .toHaveTextContent(writer.source_path)
    );
    expect(inspector).toHaveFocus();
    expect(new URL(window.location.href).searchParams.get("module"))
      .toBe(writer.source_path);
    expect(pushState).toHaveBeenCalledTimes(1);

    await user.keyboard("{Meta>}k{/Meta}");
    await user.type(
      screen.getByRole("combobox", { name: "Search repository commands" }),
      bundle.capability.views.hidden_coupling.label,
    );
    await user.keyboard("{Enter}");
    const dashboard = container.querySelector<HTMLElement>(
      "[data-repository-dashboard]",
    )!;
    await waitFor(() => expect(dashboard).toHaveFocus());
    expect(new URL(window.location.href).searchParams.get("module"))
      .toBe(writer.source_path);
    expect(new URL(window.location.href).searchParams.get("view"))
      .toBe("hidden_coupling");
    expect(pushState).toHaveBeenCalledTimes(2);

    await user.keyboard("{Meta>}k{/Meta}");
    await user.type(
      screen.getByRole("combobox", { name: "Search repository commands" }),
      "copy link",
    );
    await user.keyboard("{Enter}");
    await waitFor(() => expect(writeText).toHaveBeenCalledWith(window.location.href));
    expect(pushState).toHaveBeenCalledTimes(2);
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
