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
const goldenBundle = parseRepoBundle(
  JSON.parse(readFileSync(goldenPath, "utf8")),
);

function golden(): RepoBundle {
  return structuredClone(goldenBundle);
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

    const unrelatedNode = container.querySelector<HTMLButtonElement>(
      ".repo-graph-node.context-dimmed",
    )!;
    await user.click(unrelatedNode);
    expect(container.querySelectorAll(".repo-graph-node.context-dimmed"))
      .toHaveLength(0);
    expect(container.querySelectorAll(".repo-graph-node.coupling-focused"))
      .toHaveLength(0);
    expect(focusButtons[0]).toHaveAttribute("aria-pressed", "false");
    expect(new URL(window.location.href).searchParams.has("pair")).toBe(false);
    expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(2);
  });

  it("opens either candidate endpoint in the shared inspector and preserves the graph view", async () => {
    const bundle = golden();
    const databasePackage = bundle.graph.packages.items.find((item) =>
      item.name === "khive-db"
    )!;
    const user = userEvent.setup();
    const writeText = vi.spyOn(navigator.clipboard, "writeText")
      .mockResolvedValue(undefined);
    writeText.mockClear();
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
    const focus = within(lens).getAllByRole("button", {
      name: /Focus coupling candidate between/,
    })[0];
    await user.click(focus);
    const focusedPair = new URL(window.location.href).searchParams.getAll(
      "pair",
    );
    const endpoint = within(focus.closest("li")!).getAllByRole("button", {
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
    expect(new URL(window.location.href).searchParams.get("pkg"))
      .toBe("khive-db");
    expect(new URL(window.location.href).searchParams.get("lens"))
      .toBe("hidden_coupling");
    expect(new URL(window.location.href).searchParams.getAll("pair"))
      .toEqual(focusedPair);
    expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(2);

    await user.click(within(inspector).getByRole("button", {
      name: "Copy evidence brief",
    }));
    expect(writeText).toHaveBeenCalledOnce();
    const copied = writeText.mock.calls[0][0];
    expect(copied).toContain("Candidate hidden coupling");
    expect(copied).toContain("Observed co-change evidence");
    for (const path of focusedPair) expect(copied).toContain(path);

    writeText.mockClear();
    await user.click(screen.getByRole("button", {
      name: bundle.capability.views.scorecard.label,
    }));
    expect(new URL(window.location.href).searchParams.has("pair")).toBe(false);
    await user.click(within(inspector).getByRole("button", {
      name: "Copy evidence brief",
    }));
    expect(writeText).toHaveBeenCalledOnce();
    const offViewCopy = writeText.mock.calls[0][0];
    const retainedOnlyPath = focusedPair.find((path) => path !== sourcePath)!;
    expect(offViewCopy).not.toContain("Candidate hidden coupling");
    expect(offViewCopy).not.toContain(retainedOnlyPath);
  });

  it("opens the focused Boundary workbench and updates the inspector without stealing focus or dropping the pair", async () => {
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
    await user.click(screen.getAllByRole("button", {
      name: /Focus coupling candidate between/,
    })[0]);

    const focusedPair = new URL(window.location.href).searchParams.getAll(
      "pair",
    );
    const workbench = screen.getByRole("region", {
      name: "Boundary evidence workbench",
    });
    expect(workbench).toHaveTextContent(
      /Shared commits: 5 shown of 24 declared.*truncated/i,
    );
    expect(workbench).toHaveTextContent("crates/khive-db/src/pool.rs");
    const endpointControl = within(workbench).getByRole("button", {
      name: "Show crates/khive-db/src/stores/graph_tests.rs in module inspector",
    });

    endpointControl.focus();
    await user.click(endpointControl);

    await waitFor(() => expect(endpointControl).toHaveAttribute(
      "aria-expanded",
      "true",
    ));
    expect(endpointControl).toHaveFocus();
    expect(new URL(window.location.href).searchParams.get("module"))
      .toBe("crates/khive-db/src/stores/graph_tests.rs");
    expect(new URL(window.location.href).searchParams.getAll("pair"))
      .toEqual(focusedPair);
    expect(container.querySelector<HTMLElement>("[data-module-inspector]"))
      .toHaveTextContent("crates/khive-db/src/stores/graph_tests.rs");
  });

  it("lets pair presence drive an unavailable workbench when the focused producer row is absent", () => {
    const bundle = golden();
    const left = "crates/khive-db/src/stores/graph.rs";
    const right = "crates/khive-db/src/stores/graph_tests.rs";
    const endpointIds = new Set(bundle.graph.modules.items
      .filter((moduleNode) => [left, right].includes(moduleNode.source_path))
      .map((moduleNode) => moduleNode.id));
    bundle.aggregates.hidden_coupling.data.items =
      bundle.aggregates.hidden_coupling.data.items.filter((row) =>
        !(endpointIds.has(row.left_module_id) &&
          endpointIds.has(row.right_module_id))
      );
    window.history.replaceState(
      null,
      "",
      repositoryLocationUrl(new URL(window.location.href), {
        repository: bundle.meta.repository.canonical_url,
        snapshotSha: bundle.meta.snapshot.head_sha,
        modulePath: left,
        view: "structure_graph",
        structureGraph: {
          packageName: "khive-db",
          lens: "hidden_coupling",
          couplingPair: [left, right],
        },
      }),
    );

    render(<RepoShowcase bundle={bundle} />);

    const workbench = screen.getByRole("region", {
      name: "Boundary evidence workbench",
    });
    expect(within(workbench).getByRole("status", {
      name: "Boundary evidence status",
    })).toHaveTextContent(
      /pair_evidence_unavailable.*focused paths do not resolve to one captured coupling row/i,
    );
    expect(new URL(window.location.href).searchParams.getAll("pair"))
      .toEqual([left, right]);
  });

  it("pushes and restores the package, lens, and focused pair without relayout", async () => {
    const bundle = golden();
    const databasePackage = bundle.graph.packages.items.find((item) =>
      item.name === "khive-db"
    )!;
    const user = userEvent.setup();
    const pushState = vi.spyOn(window.history, "pushState");
    const { container } = render(<RepoShowcase bundle={bundle} />);

    await waitFor(() => expect(settleGraphLayoutSpy).toHaveBeenCalledOnce());
    const packageSelect = screen.getByRole("combobox", {
      name:
        `${bundle.capability.labels.node_types.package} · ${bundle.capability.views.structure_graph.label}`,
    });
    await user.selectOptions(packageSelect, databasePackage.id);
    expect(new URL(window.location.href).searchParams.get("pkg"))
      .toBe("khive-db");
    expect(pushState).toHaveBeenCalledTimes(1);
    expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(2);

    const hiddenLens = screen.getByRole("radio", {
      name: bundle.capability.views.hidden_coupling.label,
    });
    await user.click(hiddenLens);
    expect(new URL(window.location.href).searchParams.get("lens"))
      .toBe("hidden_coupling");
    expect(pushState).toHaveBeenCalledTimes(2);
    expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(2);

    const focus = screen.getAllByRole("button", {
      name: /Focus coupling candidate between/,
    })[0];
    const focusName = focus.getAttribute("aria-label")!;
    const [, left, right] = focusName.match(
      /^Focus coupling candidate between (.+) and (.+)$/,
    )!;
    await user.click(focus);
    const focusedUrl = window.location.href;
    expect(new URL(focusedUrl).searchParams.getAll("pair")).toEqual(
      [left, right].sort(),
    );
    expect(pushState).toHaveBeenCalledTimes(3);
    expect(container.querySelectorAll("[data-coupling-overlay].selected"))
      .toHaveLength(1);
    expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(2);

    await user.click(screen.getByRole("radio", {
      name: bundle.capability.views.structure_graph.label,
    }));
    const otherLensUrl = window.location.href;
    expect(new URL(window.location.href).searchParams.has("pair")).toBe(false);
    expect(pushState).toHaveBeenCalledTimes(4);
    expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(2);

    window.history.replaceState(null, "", focusedUrl);
    window.dispatchEvent(new PopStateEvent("popstate"));
    await waitFor(() => expect(hiddenLens).toBeChecked());
    expect(packageSelect).toHaveValue(databasePackage.id);
    await waitFor(() =>
      expect(container.querySelectorAll("[data-coupling-overlay].selected"))
        .toHaveLength(1)
    );
    const focusedAnnouncement = screen.getByRole("status", {
      name: "Investigation navigation",
    }).textContent;
    const [canonicalLeft, canonicalRight] = new URL(focusedUrl).searchParams
      .getAll("pair");
    expect(focusedAnnouncement).toContain("Package scope khive-db");
    expect(focusedAnnouncement).toContain("lens Hidden coupling");
    expect(focusedAnnouncement).toContain(
      `focused pair ${canonicalLeft} and ${canonicalRight}`,
    );
    expect(pushState).toHaveBeenCalledTimes(4);
    expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(2);

    window.history.replaceState(null, "", otherLensUrl);
    window.dispatchEvent(new PopStateEvent("popstate"));
    await waitFor(() => expect(hiddenLens).not.toBeChecked());
    const structureAnnouncement = screen.getByRole("status", {
      name: "Investigation navigation",
    }).textContent;
    expect(structureAnnouncement).toContain("Package scope khive-db");
    expect(structureAnnouncement).toContain("lens Structure graph");
    expect(structureAnnouncement).toContain("no focused pair");
    expect(structureAnnouncement).not.toBe(focusedAnnouncement);
    expect(pushState).toHaveBeenCalledTimes(4);
    expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(2);

    await user.click(screen.getByRole("button", {
      name: bundle.capability.views.scorecard.label,
    }));
    const otherViewUrl = new URL(window.location.href);
    expect(otherViewUrl.searchParams.has("pkg")).toBe(false);
    expect(otherViewUrl.searchParams.has("lens")).toBe(false);
    expect(otherViewUrl.searchParams.has("pair")).toBe(false);
    expect(pushState).toHaveBeenCalledTimes(5);
    expect(settleGraphLayoutSpy).toHaveBeenCalledTimes(2);
  });

  it("keeps a stale coupling pair pending until the current snapshot is accepted", async () => {
    const bundle = golden();
    const pair = bundle.aggregates.hidden_coupling.data.items.find((candidate) => {
      const left = bundle.graph.modules.items.find((item) =>
        item.id === candidate.left_module_id
      );
      const right = bundle.graph.modules.items.find((item) =>
        item.id === candidate.right_module_id
      );
      return left?.source_path.includes("khive-db/src/stores/graph") &&
        right?.source_path.includes("khive-db/src/stores/graph");
    })!;
    const endpoints = [pair.left_module_id, pair.right_module_id]
      .map((id) => bundle.graph.modules.items.find((item) => item.id === id)!.source_path)
      .sort() as [string, string];
    const direct = repositoryLocationUrl(new URL(window.location.href), {
      repository: bundle.meta.repository.canonical_url,
      snapshotSha: "0000000000000000000000000000000000000000",
      modulePath: endpoints[0],
      view: "structure_graph",
      structureGraph: {
        packageName: "khive-db",
        lens: "hidden_coupling",
        couplingPair: endpoints,
      },
    });
    window.history.replaceState(null, "", direct);
    const user = userEvent.setup();
    const { container } = render(<RepoShowcase bundle={bundle} />);

    expect(await screen.findByRole("radio", {
      name: bundle.capability.views.hidden_coupling.label,
    })).toBeChecked();
    expect(container.querySelectorAll("[data-coupling-overlay].selected"))
      .toHaveLength(0);
    expect(new URL(window.location.href).searchParams.getAll("pair"))
      .toEqual(endpoints);
    const notice = await screen.findByRole("status", {
      name: "Investigation link status",
    });
    expect(notice).toHaveTextContent(/coupling pair.*current snapshot/i);

    const staleHref = window.location.href;
    const pushState = vi.spyOn(window.history, "pushState");
    pushState.mockClear();
    await user.click(screen.getByRole("button", {
      name: bundle.capability.views.structure_graph.label,
    }));
    expect(window.location.href).toBe(staleHref);
    expect(pushState).not.toHaveBeenCalled();
    expect(screen.getByRole("status", {
      name: "Investigation link status",
    })).toHaveTextContent(/coupling pair.*current snapshot/i);
    expect(new URL(window.location.href).searchParams.getAll("pair"))
      .toEqual(endpoints);

    await user.click(within(notice).getByRole("button", {
      name: "Use current snapshot",
    }));
    await waitFor(() =>
      expect(container.querySelectorAll("[data-coupling-overlay].selected"))
        .toHaveLength(1)
    );
    expect(new URL(window.location.href).searchParams.get("at"))
      .toBe(bundle.meta.snapshot.head_sha);
    expect(new URL(window.location.href).searchParams.getAll("pair"))
      .toEqual(endpoints);
  });

  it("fails closed when a package scope cannot be resolved", async () => {
    const bundle = golden();
    const direct = repositoryLocationUrl(new URL(window.location.href), {
      repository: bundle.meta.repository.canonical_url,
      snapshotSha: bundle.meta.snapshot.head_sha,
      modulePath: null,
      view: "structure_graph",
      structureGraph: {
        packageName: "not-a-captured-package",
        lens: "hidden_coupling",
        couplingPair: [
          "crates/khive-db/src/stores/graph.rs",
          "crates/khive-db/src/stores/graph_tests.rs",
        ],
      },
    });
    window.history.replaceState(null, "", direct);
    render(<RepoShowcase bundle={bundle} />);

    const notice = await screen.findByRole("status", {
      name: "Investigation link status",
    });
    expect(notice).toHaveTextContent(/requested package.*not present/i);
    expect(screen.getByRole("combobox", {
      name:
        `${bundle.capability.labels.node_types.package} · ${bundle.capability.views.structure_graph.label}`,
    })).toHaveValue(bundle.graph.repository.id);
    expect(screen.getByRole("radio", {
      name: bundle.capability.views.hidden_coupling.label,
    })).toBeChecked();
    const repaired = new URL(window.location.href);
    expect(repaired.searchParams.has("pkg")).toBe(false);
    expect(repaired.searchParams.has("pair")).toBe(false);
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
