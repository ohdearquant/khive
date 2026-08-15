import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { RepoShowcase } from "@/components/showcase/repo-showcase";
import { parseRepoBundle, type RepoBundle } from "@/lib/repo-bundle";

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

function visibleModuleButtons(
  container: HTMLElement,
  bundle: RepoBundle,
): HTMLButtonElement[] {
  const moduleIds = new Set(bundle.graph.modules.items.map((module) => module.id));
  return Array.from(
    container.querySelectorAll<HTMLButtonElement>(".repo-graph-node[data-node-id]"),
  ).filter((button) => moduleIds.has(button.dataset.nodeId ?? ""));
}

describe("repository showcase graph layout", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    settleGraphLayoutSpy.mockClear();
    window.history.replaceState(null, "", "/");
    Object.defineProperty(HTMLElement.prototype, "scrollIntoView", {
      configurable: true,
      value: vi.fn(),
    });
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

  it.each(["click", "Enter"] as const)(
    "shares ordinary module selection on %s without recomputing layout",
    async (interaction) => {
      const bundle = golden();
      const user = userEvent.setup();
      const { container } = render(<RepoShowcase bundle={bundle} />);

      await waitFor(() => expect(settleGraphLayoutSpy).toHaveBeenCalledOnce());
      const targetControl = visibleModuleButtons(container, bundle).find(
        (button) => button.getAttribute("aria-pressed") === "false",
      )!;
      const target = bundle.graph.modules.items.find((module) =>
        module.id === targetControl.dataset.nodeId
      )!;
      const pushState = vi.spyOn(window.history, "pushState");

      if (interaction === "click") {
        await user.click(targetControl);
      } else {
        targetControl.focus();
        await user.keyboard("{Enter}");
      }

      const inspector = container.querySelector<HTMLElement>(
        "[data-module-inspector]",
      )!;
      await waitFor(() => expect(inspector).toHaveFocus());
      expect(within(inspector).getByRole("heading", { level: 3 }))
        .toHaveTextContent(target.source_path);
      expect(targetControl).toHaveAttribute("aria-pressed", "true");
      expect(new URL(window.location.href).searchParams.get("view"))
        .toBe("structure_graph");
      expect(new URL(window.location.href).searchParams.get("module"))
        .toBe(target.source_path);
      expect(pushState).toHaveBeenCalledOnce();
      expect(settleGraphLayoutSpy).toHaveBeenCalledOnce();
    },
  );

  it("restores a visible graph module from popstate without pushing or recomputing layout", async () => {
    const bundle = golden();
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);

    await waitFor(() => expect(settleGraphLayoutSpy).toHaveBeenCalledOnce());
    const [firstControl, secondControl] = visibleModuleButtons(container, bundle);
    const first = bundle.graph.modules.items.find((module) =>
      module.id === firstControl.dataset.nodeId
    )!;
    const second = bundle.graph.modules.items.find((module) =>
      module.id === secondControl.dataset.nodeId
    )!;
    const pushState = vi.spyOn(window.history, "pushState");
    await user.click(firstControl);
    await user.click(secondControl);
    expect(pushState).toHaveBeenCalledTimes(2);

    const restored = new URL(window.location.href);
    restored.searchParams.set("module", first.source_path);
    window.history.replaceState(null, "", restored);
    window.dispatchEvent(new PopStateEvent("popstate"));

    const inspector = container.querySelector<HTMLElement>(
      "[data-module-inspector]",
    )!;
    await waitFor(() =>
      expect(within(inspector).getByRole("heading", { level: 3 }))
        .toHaveTextContent(first.source_path)
    );
    await waitFor(() => expect(firstControl).toHaveAttribute("aria-pressed", "true"));
    expect(secondControl).toHaveAttribute("aria-pressed", "false");
    expect(pushState).toHaveBeenCalledTimes(2);
    expect(settleGraphLayoutSpy).toHaveBeenCalledOnce();
    expect(second.source_path).not.toBe(first.source_path);
  });

  it("keeps repository and package node selection local to the graph", async () => {
    const bundle = golden();
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);

    await waitFor(() => expect(settleGraphLayoutSpy).toHaveBeenCalledOnce());
    const targetControl = visibleModuleButtons(container, bundle)[0];
    const target = bundle.graph.modules.items.find((module) =>
      module.id === targetControl.dataset.nodeId
    )!;
    await user.click(targetControl);
    const moduleLocation = window.location.href;
    const pushState = vi.spyOn(window.history, "pushState");

    const repositoryControl = container.querySelector<HTMLButtonElement>(
      `.repo-graph-node[data-node-id="${bundle.graph.repository.id}"]`,
    )!;
    await user.click(repositoryControl);
    expect(repositoryControl).toHaveAttribute("aria-pressed", "true");
    expect(window.location.href).toBe(moduleLocation);
    expect(pushState).not.toHaveBeenCalled();

    const packageIds = new Set(bundle.graph.packages.items.map((item) => item.id));
    const packageControl = Array.from(
      container.querySelectorAll<HTMLButtonElement>(
        ".repo-graph-node[data-node-id]",
      ),
    ).find((button) => packageIds.has(button.dataset.nodeId ?? ""))!;
    await user.click(packageControl);
    expect(packageControl).toHaveAttribute("aria-pressed", "true");
    expect(window.location.href).toBe(moduleLocation);
    expect(pushState).not.toHaveBeenCalled();

    const inspector = container.querySelector<HTMLElement>(
      "[data-module-inspector]",
    )!;
    expect(within(inspector).getByRole("heading", { level: 3 }))
      .toHaveTextContent(target.source_path);
    expect(settleGraphLayoutSpy).toHaveBeenCalledOnce();
  });
});
