import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { RepoShowcase } from "@/components/showcase/repo-showcase";
import { parseRepoBundle, type RepoBundle } from "@/lib/repo-bundle";
import { repositoryLocationUrl } from "@/lib/repository-location";

const settleGraphLayoutSpy = vi.hoisted(() => vi.fn());

vi.mock("@/lib/graph-layout", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/graph-layout")>();
  settleGraphLayoutSpy.mockImplementation(actual.settleGraphLayout);
  return { ...actual, settleGraphLayout: settleGraphLayoutSpy };
});

const goldenPath = resolve(
  process.cwd(),
  "../../docs/schemas/examples/khive-repo-v1-khive.json",
);

function golden(): RepoBundle {
  return parseRepoBundle(JSON.parse(readFileSync(goldenPath, "utf8")));
}

describe("repository showcase graph layout", () => {
  beforeEach(() => {
    settleGraphLayoutSpy.mockClear();
    window.history.replaceState(null, "", "/");
  });

  it("reuses settled positions for local interaction and invalidates on layout input changes", async () => {
    const bundle = golden();
    const user = userEvent.setup();
    const { container, rerender } = render(<RepoShowcase bundle={bundle} />);

    await waitFor(() => expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(1));

    await user.click(screen.getByRole("button", {
      name: `${bundle.capability.views.structure_graph.label} +`,
    }));
    const graphNodes = container.querySelectorAll<HTMLButtonElement>(
      ".repo-graph-node",
    );
    expect(graphNodes.length).toBeGreaterThan(1);
    await user.click(graphNodes[1]);
    expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(1);

    const packageChoice = bundle.graph.packages.items[0];
    expect(packageChoice).toBeDefined();
    await user.selectOptions(
      screen.getByRole("combobox", {
        name:
          `${bundle.capability.labels.node_types.package} · ${bundle.capability.views.structure_graph.label}`,
      }),
      packageChoice.id,
    );
    expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(2);

    rerender(<RepoShowcase bundle={bundle} />);
    expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(2);

    // An unrelated bundle-field change (a capability label) with every graph
    // layout input reference preserved must not recompute the layout. The
    // graph container is deliberately a fresh object: a whole-`graph`
    // dependency would recompute here, while the fine-grained inputs
    // (repository id and the three item arrays) keep their identity.
    const relabeledBundle = {
      ...bundle,
      capability: {
        ...bundle.capability,
        labels: {
          ...bundle.capability.labels,
          truncated: `${bundle.capability.labels.truncated} (relabeled)`,
        },
      },
      graph: { ...bundle.graph },
    };
    rerender(<RepoShowcase bundle={relabeledBundle} />);
    expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(2);

    const replacedModulePage = {
      ...bundle,
      graph: {
        ...bundle.graph,
        modules: {
          ...bundle.graph.modules,
          items: [...bundle.graph.modules.items],
        },
      },
    };
    rerender(<RepoShowcase bundle={replacedModulePage} />);
    expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(3);
  });

  it("keeps the user's graph selection across a rerender, and only realigns it on real navigation", async () => {
    const bundle = golden();
    const user = userEvent.setup();
    const { container, rerender } = render(<RepoShowcase bundle={bundle} />);
    await waitFor(() => expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(1));

    await user.click(screen.getByRole("button", {
      name: `${bundle.capability.views.structure_graph.label} +`,
    }));

    const packageChoice = bundle.graph.packages.items[0];
    await user.selectOptions(
      screen.getByRole("combobox", {
        name:
          `${bundle.capability.labels.node_types.package} · ${bundle.capability.views.structure_graph.label}`,
      }),
      packageChoice.id,
    );
    const moduleNode = container.querySelector<HTMLButtonElement>(
      "[data-module-node]",
    )!;
    const moduleNodeId = moduleNode.getAttribute("data-node-id")!;
    await user.click(moduleNode);
    expect(
      container.querySelector(`[data-node-id="${moduleNodeId}"]`),
    ).toHaveAttribute("aria-pressed", "true");

    // A rerender that only supplies a fresh `modules` items array identity
    // (same content, new reference) must not reset the in-graph selection
    // the user just made.
    const freshModuleList = {
      ...bundle,
      graph: {
        ...bundle.graph,
        modules: {
          ...bundle.graph.modules,
          items: [...bundle.graph.modules.items],
        },
      },
    };
    rerender(<RepoShowcase bundle={freshModuleList} />);
    expect(
      screen.getByRole("combobox", {
        name:
          `${bundle.capability.labels.node_types.package} · ${bundle.capability.views.structure_graph.label}`,
      }),
    ).toHaveValue(packageChoice.id);
    expect(
      container.querySelector(`[data-node-id="${moduleNodeId}"]`),
    ).toHaveAttribute("aria-pressed", "true");

    // An in-app navigation (a view switch pushes a new location) followed by
    // another fresh-module-list rerender must not realign the graph either:
    // the selection was set explicitly and the URL merely followed it.
    await user.click(screen.getByRole("button", {
      name: bundle.capability.views.hidden_coupling.label,
    }));
    rerender(
      <RepoShowcase
        bundle={{
          ...freshModuleList,
          graph: {
            ...freshModuleList.graph,
            modules: {
              ...freshModuleList.graph.modules,
              items: [...freshModuleList.graph.modules.items],
            },
          },
        }}
      />,
    );
    await user.click(screen.getByRole("button", {
      name: bundle.capability.views.structure_graph.label,
    }));
    expect(
      screen.getByRole("combobox", {
        name:
          `${bundle.capability.labels.node_types.package} · ${bundle.capability.views.structure_graph.label}`,
      }),
    ).toHaveValue(packageChoice.id);
    expect(
      container.querySelector(`[data-node-id="${moduleNodeId}"]`),
    ).toHaveAttribute("aria-pressed", "true");

    // An actual navigation (browser back/forward) to a different module's
    // URL still realigns the graph to that module.
    const otherModule = bundle.graph.modules.items.find((module) =>
      module.package_id !== packageChoice.id
    )!;
    const target = repositoryLocationUrl(new URL(window.location.href), {
      repository: bundle.meta.repository.canonical_url,
      snapshotSha: bundle.meta.snapshot.head_sha,
      modulePath: null,
      moduleId: otherModule.id,
      view: "structure_graph",
    });
    window.history.pushState(null, "", target);
    window.dispatchEvent(new PopStateEvent("popstate"));

    await waitFor(() =>
      expect(
        screen.getByRole("combobox", {
          name:
            `${bundle.capability.labels.node_types.package} · ${bundle.capability.views.structure_graph.label}`,
        }),
      ).toHaveValue(otherModule.package_id)
    );
    expect(
      container.querySelector(`[data-node-id="${otherModule.id}"]`),
    ).toHaveAttribute("aria-pressed", "true");
  });

  it("reports the selected subtree's true totals, not the displayed slice, in the disclosure counts", async () => {
    const draft = structuredClone(golden());
    const moduleCountByPackage = new Map<string, number>();
    for (const moduleItem of draft.graph.modules.items) {
      moduleCountByPackage.set(
        moduleItem.package_id,
        (moduleCountByPackage.get(moduleItem.package_id) ?? 0) + 1,
      );
    }
    const [targetPackageId, baselineCount] = [...moduleCountByPackage.entries()]
      .sort((left, right) => right[1] - left[1])[0];
    const template = draft.graph.modules.items.find((module) =>
      module.package_id === targetPackageId
    )!;
    const extraModules = Array.from(
      { length: Math.max(0, 50 - baselineCount) },
      (_, index) => ({
        ...template,
        id: `${template.id}-subtree-cap-${index}`,
        source_path: `${template.source_path}.subtree-cap-${index}`,
      }),
    );
    draft.graph.modules.items.push(...extraModules);
    if (draft.graph.modules.total_count.status === "available") {
      draft.graph.modules.total_count.value = draft.graph.modules.items.length;
    }
    const bundle = parseRepoBundle(draft);
    const labels = bundle.capability.labels;
    const totalPackages = bundle.graph.packages.items.length;
    const totalModulesInPackage = bundle.graph.modules.items.filter((module) =>
      module.package_id === targetPackageId
    ).length;
    expect(totalPackages).toBeGreaterThan(8);
    expect(totalModulesInPackage).toBeGreaterThan(42);

    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);
    await waitFor(() => expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(1));

    const card = container.querySelector<HTMLElement>(".repo-card")!;
    const packageDisclosure = within(card)
      .getByText(`${labels.node_types.package} ${labels.truncated.toLocaleLowerCase()}`)
      .closest('[data-state="truncated"]') as HTMLElement;
    expect(packageDisclosure).toHaveAttribute("data-shown", "8");
    expect(packageDisclosure).toHaveAttribute(
      "data-known-total",
      String(totalPackages),
    );

    await user.selectOptions(
      screen.getByRole("combobox", {
        name:
          `${labels.node_types.package} · ${bundle.capability.views.structure_graph.label}`,
      }),
      targetPackageId,
    );

    const moduleDisclosure = within(card)
      .getByText(`${labels.node_types.module} ${labels.truncated.toLocaleLowerCase()}`)
      .closest('[data-state="truncated"]') as HTMLElement;
    expect(moduleDisclosure).toHaveAttribute("data-shown", "42");
    expect(moduleDisclosure).toHaveAttribute(
      "data-known-total",
      String(totalModulesInPackage),
    );
  });
});
