import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

import { parseRepoBundle, type RepoBundle } from "@/lib/repo-bundle";
import { buildStructureCouplingLens } from "@/lib/structure-coupling-lens";

const goldenPath = resolve(
  process.cwd(),
  "../../docs/schemas/examples/khive-repo-v1-khive.json",
);

function golden(): RepoBundle {
  return parseRepoBundle(JSON.parse(readFileSync(goldenPath, "utf8")));
}

function visibleModuleIds(
  bundle: RepoBundle,
  packageId: string | null,
): Set<string> {
  const packages = packageId === null
    ? [...bundle.graph.packages.items]
      .sort((left, right) => left.id.localeCompare(right.id))
      .slice(0, 8)
    : bundle.graph.packages.items.filter((item) => item.id === packageId);
  const packageIds = new Set(packages.map((item) => item.id));
  return new Set(
    bundle.graph.modules.items
      .filter((item) => packageIds.has(item.package_id))
      .sort((left, right) => left.id.localeCompare(right.id))
      .slice(0, 42)
      .map((item) => item.id),
  );
}

describe("structure hidden-coupling lens", () => {
  it("selects a deterministic bounded visible slice from the real snapshot", () => {
    const bundle = golden();
    const rootLens = buildStructureCouplingLens({
      aggregateStatus: bundle.aggregates.hidden_coupling.meta.status,
      pairPage: bundle.aggregates.hidden_coupling.data,
      structureEdgePage: bundle.graph.structure_edges,
      visibleModuleIds: visibleModuleIds(bundle, null),
      limit: 20,
    });
    expect(rootLens.capturedVisiblePairCount).toBe(10);
    expect(rootLens.pairs).toHaveLength(10);

    const databasePackage = bundle.graph.packages.items.find((item) =>
      item.name === "khive-db"
    );
    expect(databasePackage).toBeDefined();
    const databaseLens = buildStructureCouplingLens({
      aggregateStatus: bundle.aggregates.hidden_coupling.meta.status,
      pairPage: bundle.aggregates.hidden_coupling.data,
      structureEdgePage: bundle.graph.structure_edges,
      visibleModuleIds: visibleModuleIds(bundle, databasePackage!.id),
      limit: 20,
    });

    expect(databaseLens.capturedVisiblePairCount).toBe(70);
    expect(databaseLens.pairs).toHaveLength(20);
    expect(databaseLens.pairs[0]).toMatchObject({
      cochangeCount: 24,
      dependencyEvidence: "absent",
    });
    expect(databaseLens.capturedPairCount).toBe(1_000);
    expect(databaseLens.declaredPairCount).toBe(104_263);
    expect(databaseLens.coverage).toBe("truncated");
  });

  it("treats a continuation cursor as incomplete captured evidence", () => {
    const bundle = golden();
    const pairPage = {
      ...bundle.aggregates.hidden_coupling.data,
      truncated: false,
      next_cursor: "next-hidden-coupling-page",
      disclosure: {
        status: "complete" as const,
      },
    };
    const lens = buildStructureCouplingLens({
      aggregateStatus: bundle.aggregates.hidden_coupling.meta.status,
      pairPage,
      structureEdgePage: bundle.graph.structure_edges,
      visibleModuleIds: visibleModuleIds(bundle, null),
      limit: 20,
    });

    expect(lens.coverage).toBe("truncated");
    expect(lens.coverageReason).toContain("continuation cursor");
  });

  it("never turns incomplete structure evidence into an absence claim", () => {
    const bundle = golden();
    const databasePackage = bundle.graph.packages.items.find((item) =>
      item.name === "khive-db"
    )!;
    const structureEdgePage = {
      ...bundle.graph.structure_edges,
      truncated: true,
      next_cursor: "next-structure-edge-page",
      disclosure: {
        status: "truncated" as const,
        reason: "structure edge page reached its bound",
      },
    };
    const lens = buildStructureCouplingLens({
      aggregateStatus: bundle.aggregates.hidden_coupling.meta.status,
      pairPage: bundle.aggregates.hidden_coupling.data,
      structureEdgePage,
      visibleModuleIds: visibleModuleIds(bundle, databasePackage.id),
      limit: 20,
    });

    expect(lens.pairs).toHaveLength(20);
    expect(lens.pairs.every((pair) => pair.dependencyEvidence === "unknown"))
      .toBe(true);
    expect(lens.dependencyCoverageReason).toBe(
      "structure edge page reached its bound",
    );
  });

  it("reports a captured dependency without changing pair orientation", () => {
    const draft = structuredClone(golden());
    const pair = draft.aggregates.hidden_coupling.data.items[0];
    const existingEdge = draft.graph.structure_edges.items[0];
    draft.graph.structure_edges = {
      ...draft.graph.structure_edges,
      items: [{
        ...existingEdge,
        source: pair.left_module_id,
        target: pair.right_module_id,
        relation: "depends_on" as const,
      }],
      total_count: { status: "available", value: 1 },
      next_cursor: null,
      truncated: false,
      disclosure: { status: "complete" },
    };
    const bundle = parseRepoBundle(draft);
    const lens = buildStructureCouplingLens({
      aggregateStatus: bundle.aggregates.hidden_coupling.meta.status,
      pairPage: bundle.aggregates.hidden_coupling.data,
      structureEdgePage: bundle.graph.structure_edges,
      visibleModuleIds: new Set([
        pair.left_module_id,
        pair.right_module_id,
      ]),
      limit: 1,
    });

    expect(lens.pairs[0]).toMatchObject({
      leftModuleId: pair.left_module_id,
      rightModuleId: pair.right_module_id,
      dependencyEvidence: "present",
    });
  });

  it("suppresses overlays when the aggregate declares itself unavailable, even with captured rows still present", () => {
    const bundle = golden();
    const lens = buildStructureCouplingLens({
      aggregateStatus: "unavailable",
      pairPage: bundle.aggregates.hidden_coupling.data,
      structureEdgePage: bundle.graph.structure_edges,
      visibleModuleIds: visibleModuleIds(bundle, null),
      limit: 20,
    });

    expect(bundle.aggregates.hidden_coupling.data.items.length).toBeGreaterThan(0);
    expect(lens.pairs).toHaveLength(0);
    expect(lens.coverage).toBe("unavailable");
  });
});
