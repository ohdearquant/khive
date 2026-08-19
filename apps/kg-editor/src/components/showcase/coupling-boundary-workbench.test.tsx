import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { CouplingBoundaryWorkbench } from "@/components/showcase/coupling-boundary-workbench";
import {
  buildCouplingComparison,
  couplingComparisonResultStatus,
} from "@/lib/coupling-comparison";
import { parseRepoBundle, type RepoBundle } from "@/lib/repo-bundle";

const goldenPath = resolve(
  process.cwd(),
  "../../docs/schemas/examples/khive-repo-v1-khive.json",
);
const goldenBundle = parseRepoBundle(
  JSON.parse(readFileSync(goldenPath, "utf8")),
);

const graphImplementation = "crates/khive-db/src/stores/graph.rs";
const graphTests = "crates/khive-db/src/stores/graph_tests.rs";

function golden(): RepoBundle {
  return structuredClone(goldenBundle);
}

function comparison(bundle = golden()) {
  return buildCouplingComparison({
    bundle,
    sourcePaths: [graphImplementation, graphTests],
  });
}

function moduleId(bundle: RepoBundle, sourcePath: string) {
  return bundle.graph.modules.items.find((moduleNode) =>
    moduleNode.source_path === sourcePath
  )!.id;
}

describe("Boundary evidence workbench", () => {
  it("renders bounded pair evidence and two labelled endpoint articles", () => {
    const bundle = golden();
    const result = comparison(bundle);
    const onInspectModule = vi.fn();

    render(
      <CouplingBoundaryWorkbench
        result={result}
        selectedModuleId={moduleId(bundle, graphImplementation)}
        onInspectModule={onInspectModule}
      />,
    );

    const workbench = screen.getByRole("region", {
      name: "Boundary evidence workbench",
    });
    expect(within(workbench).getByRole("status", {
      name: "Boundary evidence status",
    })).toHaveTextContent(
      /Shared commits: 5 shown of 24 declared.*truncated.*Common structural neighbors: 1 shown of 1 declared.*complete/i,
    );
    expect(workbench).toHaveTextContent(/coupling candidate, not a defect/i);
    expect(workbench).toHaveTextContent("5 / 24 shown · bound 5");
    expect(workbench).toHaveTextContent(
      /19 additional shared captured commits.*fixed display bound/i,
    );
    expect(workbench).toHaveTextContent("crates/khive-db/src/pool.rs");
    expect(workbench).toHaveTextContent(
      /left outgoing depends_on.*right outgoing depends_on/i,
    );
    expect(workbench).toHaveTextContent(
      /No captured direct dependency edge.*complete structure evidence/i,
    );
    expect(workbench).toHaveTextContent(/captured contribution history/i);
    expect(workbench).toHaveTextContent("Verify next");

    const implementation = within(workbench).getByRole("article", {
      name: `Boundary endpoint ${graphImplementation}`,
    });
    expect(implementation).toHaveTextContent("Fan-in 2");
    expect(implementation).toHaveTextContent("Fan-out 4");
    expect(implementation).toHaveTextContent("38 / 38 shown · bound 50");
    expect(implementation).toHaveTextContent("high_churn_high_fan_in");
    expect(implementation).toHaveTextContent(
      "Hotspot window: 365-day analysis window (2025-08-07T18:00:00+00:00 to 2026-08-07T18:00:00+00:00)",
    );
    expect(implementation).toHaveTextContent(
      "Ownership window: Declared all-history analysis window",
    );

    const tests = within(workbench).getByRole("article", {
      name: `Boundary endpoint ${graphTests}`,
    });
    expect(tests).toHaveTextContent("Fan-in 0");
    expect(tests).toHaveTextContent("Fan-out 1");
    expect(tests).toHaveTextContent("25 / 25 shown · bound 50");
    expect(tests).toHaveTextContent("high_churn_low_fan_in");

    const selectedControl = within(implementation).getByRole("button", {
      name: `Show ${graphImplementation} in module inspector`,
    });
    const otherControl = within(tests).getByRole("button", {
      name: `Show ${graphTests} in module inspector`,
    });
    expect(selectedControl).toHaveAttribute(
      "aria-controls",
      "repository-module-inspector",
    );
    expect(selectedControl).toHaveAttribute("aria-expanded", "true");
    expect(otherControl).toHaveAttribute("aria-expanded", "false");
  });

  it("updates the shared inspector without moving focus away from the endpoint control", async () => {
    const bundle = golden();
    const onInspectModule = vi.fn();
    const user = userEvent.setup();
    render(
      <CouplingBoundaryWorkbench
        result={comparison(bundle)}
        selectedModuleId={moduleId(bundle, graphImplementation)}
        onInspectModule={onInspectModule}
      />,
    );
    const control = screen.getByRole("button", {
      name: `Show ${graphTests} in module inspector`,
    });

    control.focus();
    await user.click(control);

    expect(onInspectModule).toHaveBeenCalledOnce();
    expect(onInspectModule).toHaveBeenCalledWith(
      moduleId(bundle, graphTests),
    );
    expect(control).toHaveFocus();
  });

  it("renders a text-status unknown instead of endpoint claims when revision binding fails", () => {
    const bundle = golden();
    bundle.graph.modules.items.find((moduleNode) =>
      moduleNode.source_path === graphTests
    )!.source_revision = "f".repeat(40);

    const result = comparison(bundle);
    expect(result.status).toBe("unavailable");

    render(
      <CouplingBoundaryWorkbench
        result={result}
        selectedModuleId={null}
        onInspectModule={vi.fn()}
      />,
    );

    const workbench = screen.getByRole("region", {
      name: "Boundary evidence workbench",
    });
    expect(within(workbench).getByRole("status", {
      name: "Boundary evidence status",
    })).toHaveTextContent(couplingComparisonResultStatus(result));
    if (result.status !== "unavailable") return;
    expect(workbench).toHaveTextContent(result.code);
    expect(workbench).toHaveTextContent(result.reason);
    expect(workbench).toHaveTextContent(
      /does not match the recorded snapshot SHA/i,
    );
    expect(within(workbench).queryAllByRole("article")).toHaveLength(0);
  });

  it("renders only the bounded SCC member sample and its exact omission disclosure", () => {
    const bundle = golden();
    const endpoint = bundle.graph.modules.items.find((moduleNode) =>
      moduleNode.source_path === graphImplementation
    )!;
    const extraMembers = bundle.graph.modules.items.filter((moduleNode) =>
      ![graphImplementation, graphTests, "crates/khive-db/src/pool.rs"].includes(
        moduleNode.source_path,
      )
    ).slice(0, 11);
    const memberIds = [endpoint.id, ...extraMembers.map((item) => item.id)];
    const topology = bundle.aggregates.dependency_topology.modules.items.find(
      (row) => row.module_id === endpoint.id,
    )!;
    topology.cycle_ids = ["large-workbench-cycle"];
    bundle.aggregates.dependency_topology.cycles.items.push({
      id: "large-workbench-cycle",
      module_ids: memberIds,
    });

    render(
      <CouplingBoundaryWorkbench
        result={comparison(bundle)}
        selectedModuleId={null}
        onInspectModule={vi.fn()}
      />,
    );

    const workbench = screen.getByRole("region", {
      name: "Boundary evidence workbench",
    });
    expect(workbench).toHaveTextContent("SCC members: 6 / 12 shown · bound 6");
    expect(workbench).toHaveTextContent(
      /6 additional captured SCC members.*fixed display bound/i,
    );
    expect(workbench).not.toHaveTextContent(extraMembers[5].source_path);
  });

  it("shows why direct dependency is unknown when the source page is incomplete", () => {
    const bundle = golden();
    bundle.graph.structure_edges.truncated = true;
    bundle.graph.structure_edges.next_cursor = "next-structure-page";
    bundle.graph.structure_edges.disclosure = {
      status: "truncated",
      reason: "structure export reached its bound",
    };

    render(
      <CouplingBoundaryWorkbench
        result={comparison(bundle)}
        selectedModuleId={null}
        onInspectModule={vi.fn()}
      />,
    );

    const workbench = screen.getByRole("region", {
      name: "Boundary evidence workbench",
    });
    expect(workbench).toHaveTextContent(/Direct dependency unknown/i);
    expect(workbench).toHaveTextContent("structure export reached its bound");
    expect(workbench).not.toHaveTextContent(
      /No captured direct dependency edge.*complete structure evidence/i,
    );
  });

  it("announces shown, unknown declared totals, and truncation in the text status", () => {
    const bundle = golden();
    const leftId = moduleId(bundle, graphImplementation);
    const leftHistory = bundle.graph.history_navigation.by_module.items.find(
      (row) => row.module_id === leftId,
    )!;
    leftHistory.commits.truncated = true;
    leftHistory.commits.next_cursor = "next-history-page";
    leftHistory.commits.disclosure = {
      status: "truncated",
      reason: "history page reached its bound",
    };
    bundle.graph.modules.truncated = true;
    bundle.graph.modules.next_cursor = "next-module-page";
    bundle.graph.modules.disclosure = {
      status: "truncated",
      reason: "module page reached its bound",
    };

    render(
      <CouplingBoundaryWorkbench
        result={comparison(bundle)}
        selectedModuleId={null}
        onInspectModule={vi.fn()}
      />,
    );

    const status = screen.getByRole("status", {
      name: "Boundary evidence status",
    });
    expect(status).toHaveTextContent(
      /Shared commits: 5 shown of unknown declared.*truncated/i,
    );
    expect(status).toHaveTextContent(
      /Common structural neighbors: 1 shown of unknown declared.*truncated/i,
    );
    expect(status).not.toHaveTextContent(/5 shared captured commits/i);
  });
});
