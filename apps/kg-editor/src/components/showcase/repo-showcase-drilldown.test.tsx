import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { RepoShowcase } from "@/components/showcase/repo-showcase";
import { buildModuleInsight, buildRepositoryBrief } from "@/lib/repository-brief";
import {
  parseRepoBundle,
  type RepoBundle,
  type ViewId,
} from "@/lib/repo-bundle";

const goldenPath = resolve(
  process.cwd(),
  "../../docs/schemas/examples/khive-repo-v1-khive.json",
);

const drilldownViews = [
  "history_structure_navigation",
  "dependency_topology",
  "hotspot_quadrant",
  "hidden_coupling",
  "structure_treemap",
  "ownership",
  "api_surface",
  "scorecard",
] as const satisfies readonly ViewId[];

function golden(): RepoBundle {
  return parseRepoBundle(JSON.parse(readFileSync(goldenPath, "utf8")));
}

function chooseCapturedModule(
  bundle: RepoBundle,
  candidates: readonly string[],
): string {
  const captured = new Set(bundle.graph.modules.items.map((module) => module.id));
  const defaultId = buildRepositoryBrief(bundle).startHere[0]?.moduleId ??
    bundle.graph.modules.items[0]?.id;
  const candidate = candidates.find((id) => id !== defaultId && captured.has(id)) ??
    candidates.find((id) => captured.has(id));
  if (!candidate) throw new Error("fixture has no captured module candidate");
  return candidate;
}

function targetForView(bundle: RepoBundle, view: typeof drilldownViews[number]): string {
  if (view === "history_structure_navigation") {
    return chooseCapturedModule(
      bundle,
      bundle.graph.modules.items.map((module) => module.id),
    );
  }
  if (view === "dependency_topology") {
    return chooseCapturedModule(
      bundle,
      bundle.aggregates.dependency_topology.modules.items.map((row) =>
        row.module_id
      ),
    );
  }
  if (view === "hotspot_quadrant") {
    return chooseCapturedModule(
      bundle,
      bundle.aggregates.hotspot_quadrant.data.items.map((row) => row.module_id),
    );
  }
  if (view === "hidden_coupling") {
    return chooseCapturedModule(
      bundle,
      bundle.aggregates.hidden_coupling.data.items.flatMap((row) => [
        row.right_module_id,
        row.left_module_id,
      ]),
    );
  }
  if (view === "structure_treemap") {
    return chooseCapturedModule(
      bundle,
      bundle.aggregates.structure_treemap.data.items.map((row) => row.module_id),
    );
  }
  if (view === "ownership") {
    return chooseCapturedModule(
      bundle,
      bundle.aggregates.ownership.modules.items.map((row) => row.module_id),
    );
  }
  if (view === "api_surface") {
    return chooseCapturedModule(
      bundle,
      bundle.aggregates.api_surface.data.items.map((row) => row.module_id),
    );
  }
  const ids = bundle.aggregates.scorecard.fields.flatMap((field) =>
    field.value.status === "available" &&
      field.value.value.value_kind === "module_ids"
      ? field.value.value.value.items
      : []
  );
  return chooseCapturedModule(bundle, ids);
}

describe("repository showcase analysis drilldown", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    window.history.replaceState(null, "", "/");
    Object.defineProperty(HTMLElement.prototype, "scrollIntoView", {
      configurable: true,
      value: vi.fn(),
    });
  });

  it.each(drilldownViews)(
    "opens a %s module result in the shared inspector without changing views",
    async (view) => {
      const bundle = golden();
      const targetId = targetForView(bundle, view);
      const target = bundle.graph.modules.items.find((module) =>
        module.id === targetId
      )!;
      const user = userEvent.setup();
      const { container } = render(<RepoShowcase bundle={bundle} />);

      await user.click(screen.getByRole("button", {
        name: bundle.capability.views[view].label,
      }));
      const pushState = vi.spyOn(window.history, "pushState");
      pushState.mockClear();
      const panel = container.querySelector<HTMLElement>(".repo-view-panel")!;
      const control = within(panel).getAllByRole("button", {
        name: `Inspect ${target.source_path}`,
      })[0];
      await user.click(control);

      const inspector = container.querySelector<HTMLElement>(
        "[data-module-inspector]",
      )!;
      await waitFor(() => expect(inspector).toHaveFocus());
      expect(within(inspector).getByRole("heading", { level: 3 }))
        .toHaveTextContent(target.source_path);
      expect(control).toHaveAttribute("aria-pressed", "true");
      expect(new URL(window.location.href).searchParams.get("view")).toBe(view);
      expect(new URL(window.location.href).searchParams.get("module"))
        .toBe(target.source_path);
      expect(pushState).toHaveBeenCalledOnce();

      await user.click(control);
      expect(inspector).toHaveFocus();
      expect(pushState).toHaveBeenCalledOnce();
    },
  );

  it("lets each coupling side and SCC member select its own module", async () => {
    const bundle = golden();
    const coupling = bundle.aggregates.hidden_coupling.data.items.find((row) =>
      row.left_module_id !== row.right_module_id
    )!;
    const left = bundle.graph.modules.items.find((module) =>
      module.id === coupling.left_module_id
    )!;
    const right = bundle.graph.modules.items.find((module) =>
      module.id === coupling.right_module_id
    )!;
    const cycle = bundle.aggregates.dependency_topology.cycles.items.find(
      (row) => row.module_ids.length > 1,
    )!;
    const cycleMember = bundle.graph.modules.items.find((module) =>
      module.id === cycle.module_ids[1]
    )!;
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);
    const inspector = container.querySelector<HTMLElement>(
      "[data-module-inspector]",
    )!;

    await user.click(screen.getByRole("button", {
      name: bundle.capability.views.hidden_coupling.label,
    }));
    let panel = container.querySelector<HTMLElement>(".repo-view-panel")!;
    const couplingRow = within(panel).getAllByRole("button", {
      name: `Inspect ${right.source_path}`,
    })
      .map((button) => button.closest("tr")!)
      .find((row) =>
        within(row).queryByRole("button", {
          name: `Inspect ${left.source_path}`,
        })
      )!;

    await user.click(within(couplingRow).getByRole("button", {
      name: `Inspect ${right.source_path}`,
    }));
    expect(new URL(window.location.href).searchParams.get("module"))
      .toBe(right.source_path);
    await waitFor(() =>
      expect(within(inspector).getByRole("heading", { level: 3 }))
        .toHaveTextContent(right.source_path)
    );

    await user.click(within(couplingRow).getByRole("button", {
      name: `Inspect ${left.source_path}`,
    }));
    expect(new URL(window.location.href).searchParams.get("module"))
      .toBe(left.source_path);
    await waitFor(() =>
      expect(within(inspector).getByRole("heading", { level: 3 }))
        .toHaveTextContent(left.source_path)
    );

    await user.click(screen.getByRole("button", {
      name: bundle.capability.views.dependency_topology.label,
    }));
    panel = container.querySelector<HTMLElement>(".repo-view-panel")!;
    const cycleRow = within(panel).getByText(cycle.id).closest(".repo-list-row")!;
    await user.click(within(cycleRow as HTMLElement).getByRole("button", {
      name: `Inspect ${cycleMember.source_path}`,
    }));
    expect(new URL(window.location.href).searchParams.get("module"))
      .toBe(cycleMember.source_path);
  });

  it.each([
    ["truncated", "truncated", "outside the captured module page"],
    ["unavailable", "unavailable", "module page is unavailable"],
    ["complete", "complete", "bundle integrity mismatch"],
  ] as const)(
    "renders a non-interactive %s state for an unresolved aggregate module",
    async (_label, status, expectedReason) => {
      const draft = structuredClone(golden());
      const missingId = "module:not-captured";
      draft.aggregates.api_surface.data.items[0].module_id = missingId;
      draft.graph.modules.truncated = status === "truncated";
      draft.graph.modules.next_cursor = status === "truncated"
        ? "next-module-page"
        : null;
      draft.graph.modules.disclosure = status === "truncated"
        ? { status, reason: "module page reached its bound" }
        : status === "unavailable"
        ? { status, reason: "module export was disabled" }
        : { status };
      if (status === "unavailable") {
        draft.graph.modules.items = [];
        draft.graph.modules.total_count = {
          status: "unavailable",
          reason: "module export was disabled",
        };
      }
      const bundle = parseRepoBundle(draft);
      const user = userEvent.setup();
      const { container } = render(<RepoShowcase bundle={bundle} />);

      await user.click(screen.getByRole("button", {
        name: bundle.capability.views.api_surface.label,
      }));
      const panel = container.querySelector<HTMLElement>(".repo-view-panel")!;
      const missing = panel.querySelector<HTMLElement>(
        `[data-missing-module-id="${missingId}"]`,
      );
      expect(missing).toHaveTextContent(expectedReason);
      expect(
        panel.querySelector(`[data-module-id="${missingId}"]`),
      ).not.toBeInTheDocument();
      expect(within(panel).queryByRole("button", {
        name: `Inspect ${missingId}`,
      })).not.toBeInTheDocument();
      expect(within(panel).queryByRole("link", {
        name: `Inspect ${missingId}`,
      })).not.toBeInTheDocument();
    },
  );

  it("does not invent module drilldown for cadence-only results", async () => {
    const bundle = golden();
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);
    await user.click(screen.getByRole("button", {
      name: bundle.capability.views.cadence_timeline.label,
    }));
    const panel = container.querySelector<HTMLElement>(".repo-view-panel")!;
    expect(within(panel).queryByRole("button", { name: /^Inspect / }))
      .not.toBeInTheDocument();
  });

  it("follows the inspector's own fan-in, fan-out, and hidden-coupling links to their modules", async () => {
    const bundle = golden();
    const labels = bundle.capability.labels;
    const target = bundle.graph.modules.items.find((module) => {
      const insight = buildModuleInsight(bundle, module.id);
      return insight != null &&
        insight.dependents.length > 0 &&
        insight.dependencies.length > 0 &&
        insight.couplings.length > 0;
    });
    if (!target) {
      throw new Error(
        "fixture has no module with fan-in, fan-out, and hidden-coupling evidence",
      );
    }
    const insight = buildModuleInsight(bundle, target.id)!;
    const dependent = insight.dependents[0];
    const dependency = insight.dependencies[0];
    const coupling = insight.couplings[0];

    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);
    const inspector = container.querySelector<HTMLElement>(
      "[data-module-inspector]",
    )!;

    const search = screen.getByRole("searchbox", { name: "Find a module or path" });
    await user.clear(search);
    await user.type(search, target.source_path);
    await user.click(within(screen.getByLabelText("Module search results"))
      .getByRole("button", { name: `Inspect ${target.source_path}` }));
    await waitFor(() =>
      expect(within(inspector).getByRole("heading", { level: 3 }))
        .toHaveTextContent(target.source_path)
    );

    const fanInSection = within(inspector)
      .getByRole("heading", { level: 4, name: labels.metrics.fan_in })
      .closest("section")!;
    await user.click(within(fanInSection).getByRole("button", {
      name: dependent.source_path,
    }));
    await waitFor(() =>
      expect(within(inspector).getByRole("heading", { level: 3 }))
        .toHaveTextContent(dependent.source_path)
    );
    expect(new URL(window.location.href).searchParams.get("module"))
      .toBe(dependent.source_path);

    await user.clear(search);
    await user.type(search, target.source_path);
    await user.click(within(screen.getByLabelText("Module search results"))
      .getByRole("button", { name: `Inspect ${target.source_path}` }));
    await waitFor(() =>
      expect(within(inspector).getByRole("heading", { level: 3 }))
        .toHaveTextContent(target.source_path)
    );
    const fanOutSection = within(inspector)
      .getByRole("heading", { level: 4, name: labels.metrics.fan_out })
      .closest("section")!;
    await user.click(within(fanOutSection).getByRole("button", {
      name: dependency.source_path,
    }));
    await waitFor(() =>
      expect(within(inspector).getByRole("heading", { level: 3 }))
        .toHaveTextContent(dependency.source_path)
    );
    expect(new URL(window.location.href).searchParams.get("module"))
      .toBe(dependency.source_path);

    await user.clear(search);
    await user.type(search, target.source_path);
    await user.click(within(screen.getByLabelText("Module search results"))
      .getByRole("button", { name: `Inspect ${target.source_path}` }));
    await waitFor(() =>
      expect(within(inspector).getByRole("heading", { level: 3 }))
        .toHaveTextContent(target.source_path)
    );
    const couplingSection = within(inspector)
      .getByRole("heading", {
        level: 4,
        name: bundle.capability.views.hidden_coupling.label,
      })
      .closest("section")!;
    await user.click(within(couplingSection).getByRole("button", {
      name: coupling.module.source_path,
    }));
    await waitFor(() =>
      expect(within(inspector).getByRole("heading", { level: 3 }))
        .toHaveTextContent(coupling.module.source_path)
    );
    expect(new URL(window.location.href).searchParams.get("module"))
      .toBe(coupling.module.source_path);
  });
});
