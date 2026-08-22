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

function captureNodePositions(
  container: HTMLElement,
): Map<string, { left: string; top: string }> {
  const positions = new Map<string, { left: string; top: string }>();
  for (
    const node of container.querySelectorAll<HTMLElement>(
      ".repo-graph-node[data-node-id]",
    )
  ) {
    positions.set(node.dataset.nodeId!, {
      left: node.style.left,
      top: node.style.top,
    });
  }
  return positions;
}

describe("repository showcase hidden-coupling lens", () => {
  beforeEach(() => {
    settleGraphLayoutSpy.mockClear();
    window.history.replaceState(null, "", "/");
    Object.defineProperty(HTMLElement.prototype, "scrollIntoView", {
      configurable: true,
      value: vi.fn(),
    });
  });

  it("shows bounded candidate evidence without changing the ontology hue or layout", async () => {
    const bundle = golden();
    const databasePackage = bundle.graph.packages.items.find((item) =>
      item.name === "khive-db"
    )!;
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);

    await waitFor(() => expect(settleGraphLayoutSpy).toHaveBeenCalledOnce());
    await user.selectOptions(
      screen.getByRole("combobox", {
        name:
          `${bundle.capability.labels.node_types.package} · ${bundle.capability.views.structure_graph.label}`,
      }),
      databasePackage.id,
    );
    expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(2);

    const sampleNode = container.querySelector<HTMLElement>(
      ".repo-graph-node[data-node-id]",
    )!;
    const kindHue = sampleNode.style.getPropertyValue("--ontology-kind-hue");
    const positionsBeforeLens = captureNodePositions(container);
    await user.click(screen.getByRole("radio", {
      name: bundle.capability.views.hidden_coupling.label,
    }));

    expect(screen.getByRole("region", {
      name: `${bundle.capability.views.hidden_coupling.label} lens`,
    })).toHaveTextContent("20 of 70 captured visible pairs shown");
    expect(screen.getByRole("region", {
      name: `${bundle.capability.views.hidden_coupling.label} lens`,
    })).toHaveTextContent("365-day analysis window");
    expect(screen.getByRole("region", {
      name: `${bundle.capability.views.hidden_coupling.label} lens`,
    })).toHaveTextContent("1,000 captured of 104,263 declared");
    expect(container.querySelectorAll("[data-coupling-overlay]")).toHaveLength(
      20,
    );
    expect(sampleNode.style.getPropertyValue("--ontology-kind-hue")).toBe(
      kindHue,
    );
    expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(2);
    expect(captureNodePositions(container)).toEqual(positionsBeforeLens);

    const focusButtons = screen.getAllByRole("button", {
      name: /Focus coupling candidate between/,
    });
    await user.click(focusButtons[0]);
    expect(container.querySelectorAll(".repo-graph-node.context-dimmed").length)
      .toBeGreaterThan(0);
    expect(container.querySelectorAll(".repo-graph-node.coupling-focused"))
      .toHaveLength(2);
    expect(container.querySelectorAll("[data-coupling-overlay].selected"))
      .toHaveLength(1);
    expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(2);
    expect(captureNodePositions(container)).toEqual(positionsBeforeLens);

    const unrelatedNode = container.querySelector<HTMLButtonElement>(
      ".repo-graph-node.context-dimmed",
    )!;
    await user.click(unrelatedNode);
    expect(container.querySelectorAll(".repo-graph-node.context-dimmed"))
      .toHaveLength(0);
    expect(container.querySelectorAll(".repo-graph-node.coupling-focused"))
      .toHaveLength(0);
    expect(focusButtons[0]).toHaveAttribute("aria-pressed", "false");
    expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(2);
  });

  it("opens either candidate endpoint in the shared inspector and preserves the graph view", async () => {
    const bundle = golden();
    const databasePackage = bundle.graph.packages.items.find((item) =>
      item.name === "khive-db"
    )!;
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);

    await user.selectOptions(
      screen.getByRole("combobox", {
        name:
          `${bundle.capability.labels.node_types.package} · ${bundle.capability.views.structure_graph.label}`,
      }),
      databasePackage.id,
    );
    await user.click(screen.getByRole("radio", {
      name: bundle.capability.views.hidden_coupling.label,
    }));
    const lens = screen.getByRole("region", {
      name: `${bundle.capability.views.hidden_coupling.label} lens`,
    });
    const endpoint = within(lens).getAllByRole("button", {
      name: /Inspect crates\/khive-db\/src\/stores\/graph(_tests)?\.rs/,
    })[0];
    const sourcePath = endpoint.getAttribute("aria-label")!.replace(
      "Inspect ",
      "",
    );
    await user.click(endpoint);

    const inspector = container.querySelector<HTMLElement>(
      "[data-module-inspector]",
    )!;
    await waitFor(() => expect(inspector).toHaveFocus());
    expect(new URL(window.location.href).searchParams.get("view"))
      .toBe("structure_graph");
    expect(new URL(window.location.href).searchParams.get("module"))
      .toBe(sourcePath);
    expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(2);
  });

  it("surfaces an unavailable aggregate instead of drawing empty evidence", async () => {
    const draft = structuredClone(golden());
    draft.capability.views.hidden_coupling.status = "unavailable";
    draft.capability.views.hidden_coupling.unavailable_reason =
      "co-change export was disabled";
    draft.aggregates.hidden_coupling.meta.status = "unavailable";
    draft.aggregates.hidden_coupling.data = {
      ...draft.aggregates.hidden_coupling.data,
      items: [],
      total_count: {
        status: "unavailable",
        reason: "co-change export was disabled",
      },
      next_cursor: null,
      truncated: false,
      disclosure: {
        status: "unavailable",
        reason: "co-change export was disabled",
      },
    };
    const bundle = parseRepoBundle(draft);
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);

    await user.click(screen.getByRole("radio", {
      name: bundle.capability.views.hidden_coupling.label,
    }));
    const lens = screen.getByRole("region", {
      name: `${bundle.capability.views.hidden_coupling.label} lens`,
    });
    expect(lens).toHaveTextContent("co-change export was disabled");
    expect(container.querySelectorAll("[data-coupling-overlay]")).toHaveLength(
      0,
    );
  });
});
