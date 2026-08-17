import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { describe, expect, it } from "vitest";

import {
  buildCouplingComparison,
  COUPLING_COMMON_NEIGHBOR_LIMIT,
  COUPLING_SHARED_COMMIT_LIMIT,
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
const pool = "crates/khive-db/src/pool.rs";

function golden(): RepoBundle {
  return structuredClone(goldenBundle);
}

function comparison(bundle = golden()) {
  return buildCouplingComparison({
    bundle,
    sourcePaths: [graphImplementation, graphTests],
  });
}

function moduleByPath(bundle: RepoBundle, sourcePath: string) {
  return bundle.graph.modules.items.find((moduleNode) =>
    moduleNode.source_path === sourcePath
  )!;
}

describe("bounded coupling comparison", () => {
  it("models the real graph_tests ↔ graph boundary without turning a candidate into a defect", () => {
    const bundle = golden();
    const result = comparison(bundle);

    expect(result.status).toBe("available");
    if (result.status !== "available") return;
    const model = result.value;

    expect(model.sourceRevision).toBe(bundle.meta.snapshot.head_sha);
    expect(model.endpoints.map((endpoint) => endpoint.module.source_path))
      .toEqual([graphImplementation, graphTests]);
    expect(model.cochange).toMatchObject({
      count: 24,
      support: 0.0255863539445629,
      state: "present",
    });

    const implementation = model.endpoints[0];
    const tests = model.endpoints[1];
    expect(implementation.topology).toMatchObject({
      state: "present",
      fanIn: 2,
      fanOut: 4,
    });
    expect(tests.topology).toMatchObject({
      state: "present",
      fanIn: 0,
      fanOut: 1,
    });
    expect(implementation.scc.state).toBe("absent");
    expect(tests.scc.state).toBe("absent");
    expect(implementation.history.boundary).toMatchObject({
      status: "complete",
      shown: 38,
      declared: 38,
      bound: 50,
    });
    expect(tests.history.boundary).toMatchObject({
      status: "complete",
      shown: 25,
      declared: 25,
      bound: 50,
    });
    expect(implementation.hotspot).toMatchObject({
      state: "present",
      commitCount: 38,
      fanIn: 2,
      quadrant: "high_churn_high_fan_in",
      window: bundle.aggregates.hotspot_quadrant.meta.window,
    });
    expect(tests.hotspot).toMatchObject({
      state: "present",
      commitCount: 25,
      fanIn: 0,
      quadrant: "high_churn_low_fan_in",
    });
    expect(model.endpoints.every((endpoint) =>
      /captured contribution history/i.test(endpoint.ownership.caveat)
    )).toBe(true);
    expect(implementation.ownership.window).toEqual(
      bundle.aggregates.ownership.meta.window,
    );

    expect(model.sharedCommits.state).toBe("present");
    expect(model.sharedCommits.items).toHaveLength(
      COUPLING_SHARED_COMMIT_LIMIT,
    );
    expect(model.sharedCommits.boundary).toMatchObject({
      status: "truncated",
      shown: COUPLING_SHARED_COMMIT_LIMIT,
      declared: 24,
      bound: COUPLING_SHARED_COMMIT_LIMIT,
    });
    expect(model.sharedCommits.boundary.reason).toMatch(
      /19 additional shared captured commits.*fixed display bound/i,
    );

    expect(model.commonNeighbors.state).toBe("present");
    expect(model.commonNeighbors.boundary).toMatchObject({
      status: "complete",
      shown: 1,
      declared: 1,
      bound: COUPLING_COMMON_NEIGHBOR_LIMIT,
      reason: null,
    });
    expect(model.commonNeighbors.items).toHaveLength(1);
    expect(model.commonNeighbors.items[0]).toMatchObject({
      leftDirection: "outgoing",
      leftRelation: "depends_on",
      rightDirection: "outgoing",
      rightRelation: "depends_on",
    });
    expect(model.commonNeighbors.items[0].module.source_path).toBe(pool);

    expect(model.directDependency).toMatchObject({
      state: "absent",
      directions: [],
      boundary: {
        status: "complete",
        shown: 0,
        declared: 0,
      },
    });
    expect(model.caveat).toMatch(/candidate.*not a defect/i);
    expect(model.verifyPrompts).not.toHaveLength(0);
    expect(model.verifyPrompts.join(" ")).not.toMatch(
      /consolidat|duplicat|runtime|source[- ]role|production/i,
    );

    expect(buildCouplingComparison({
      bundle,
      sourcePaths: [graphTests, graphImplementation],
    })).toEqual(result);
  });

  it("keeps captured common-neighbor presence but makes direct-edge absence unknown when structure coverage is partial", () => {
    const bundle = golden();
    bundle.graph.structure_edges.truncated = true;
    bundle.graph.structure_edges.next_cursor = "next-structure-page";
    bundle.graph.structure_edges.disclosure = {
      status: "truncated",
      reason: "structure export reached its bound",
    };

    const result = comparison(bundle);

    expect(result.status).toBe("available");
    if (result.status !== "available") return;
    expect(result.value.commonNeighbors.state).toBe("present");
    expect(result.value.commonNeighbors.boundary).toMatchObject({
      status: "truncated",
      shown: 1,
      declared: null,
      reason: "structure export reached its bound",
    });
    expect(result.value.directDependency).toMatchObject({
      state: "unknown",
      boundary: {
        status: "truncated",
        shown: 0,
        declared: null,
        reason: "structure export reached its bound",
      },
    });
  });

  it("retains a captured direct edge as present under partial source coverage", () => {
    const bundle = golden();
    const left = moduleByPath(bundle, graphImplementation);
    const right = moduleByPath(bundle, graphTests);
    bundle.graph.structure_edges.items.push({
      id: "captured-direct-dependency",
      source: left.id,
      target: right.id,
      relation: "depends_on",
      weight: 1,
      origin: "ingested",
      derivation: null,
    });
    bundle.graph.structure_edges.truncated = true;
    bundle.graph.structure_edges.next_cursor = "next-structure-page";
    bundle.graph.structure_edges.disclosure = {
      status: "truncated",
      reason: "more structure rows exist",
    };

    const result = comparison(bundle);

    expect(result.status).toBe("available");
    if (result.status !== "available") return;
    expect(result.value.directDependency).toMatchObject({
      state: "present",
      directions: ["left_to_right"],
      boundary: {
        status: "truncated",
        shown: 1,
        declared: null,
        reason: "more structure rows exist",
      },
    });
  });

  it("uses unknown rather than absent when a partial history page has no captured intersection", () => {
    const bundle = golden();
    const left = moduleByPath(bundle, graphImplementation);
    const history = bundle.graph.history_navigation.by_module.items.find(
      (row) => row.module_id === left.id,
    )!;
    history.commits.items = ["not-shared-in-captured-page"];
    history.commits.truncated = true;
    history.commits.next_cursor = "next-history-page";
    history.commits.disclosure = {
      status: "truncated",
      reason: "history page reached its bound",
    };

    const result = comparison(bundle);

    expect(result.status).toBe("available");
    if (result.status !== "available") return;
    expect(result.value.sharedCommits).toMatchObject({
      state: "unknown",
      items: [],
      boundary: {
        status: "truncated",
        shown: 0,
        declared: null,
        reason: "history page reached its bound",
      },
    });
  });

  it("surfaces unavailable hotspot and ownership sources instead of inventing empty evidence", () => {
    const bundle = golden();
    bundle.aggregates.hotspot_quadrant.meta.status = "unavailable";
    bundle.aggregates.hotspot_quadrant.meta.unavailable_reason =
      "hotspot analysis was not produced";
    bundle.aggregates.hotspot_quadrant.data.items = [];
    bundle.aggregates.hotspot_quadrant.data.total_count = {
      status: "unavailable",
      reason: "hotspot analysis was not produced",
    };
    bundle.aggregates.hotspot_quadrant.data.disclosure = {
      status: "unavailable",
      reason: "hotspot analysis was not produced",
    };
    bundle.aggregates.ownership.meta.status = "unavailable";
    bundle.aggregates.ownership.meta.unavailable_reason =
      "ownership analysis was not produced";
    bundle.aggregates.ownership.modules.items = [];
    bundle.aggregates.ownership.modules.total_count = {
      status: "unavailable",
      reason: "ownership analysis was not produced",
    };
    bundle.aggregates.ownership.modules.disclosure = {
      status: "unavailable",
      reason: "ownership analysis was not produced",
    };

    const result = comparison(bundle);

    expect(result.status).toBe("available");
    if (result.status !== "available") return;
    for (const endpoint of result.value.endpoints) {
      expect(endpoint.hotspot).toMatchObject({
        state: "unknown",
        boundary: {
          status: "unavailable",
          reason: "hotspot analysis was not produced",
        },
      });
      expect(endpoint.ownership).toMatchObject({
        state: "unknown",
        boundary: {
          status: "unavailable",
          reason: "ownership analysis was not produced",
        },
      });
    }
  });

  it("fails closed when an endpoint or referenced common neighbor is not bound to the recorded revision", () => {
    const endpointMismatch = golden();
    moduleByPath(endpointMismatch, graphTests).source_revision = "f".repeat(40);

    expect(comparison(endpointMismatch)).toMatchObject({
      status: "unavailable",
      code: "source_revision_mismatch",
    });

    const neighborMismatch = golden();
    moduleByPath(neighborMismatch, pool).source_revision = "e".repeat(40);

    expect(comparison(neighborMismatch)).toMatchObject({
      status: "unavailable",
      code: "source_revision_mismatch",
      reason: expect.stringContaining(pool),
    });
  });

  it("filters shared navigation intersections to the producer-declared coupling window", () => {
    const bundle = golden();
    const left = moduleByPath(bundle, graphImplementation);
    const right = moduleByPath(bundle, graphTests);
    const oldCommitId = "khive:commit:outside-coupling-window";
    const template = bundle.graph.commits.items[0];
    bundle.graph.commits.items.push({
      ...template,
      id: oldCommitId,
      sha: "a".repeat(40),
      short_sha: "a".repeat(8),
      committed_at: "2024-01-01T00:00:00Z",
      subject: "outside the captured coupling window",
    });
    for (const endpoint of [left, right]) {
      const history = bundle.graph.history_navigation.by_module.items.find(
        (row) => row.module_id === endpoint.id,
      )!;
      history.commits.items.push(oldCommitId);
      if (history.commits.total_count.status === "available") {
        history.commits.total_count.value += 1;
      }
    }

    const result = comparison(bundle);

    expect(result.status).toBe("available");
    if (result.status !== "available") return;
    expect(result.value.sharedCommits.boundary.declared).toBe(24);
    expect(result.value.sharedCommits.items.map((item) => item.id))
      .not.toContain(oldCommitId);
  });

  it("makes shared-commit coverage unknown when the only intersection has no captured commit record", () => {
    const bundle = golden();
    const missingId = "khive:commit:missing-record";
    for (const sourcePath of [graphImplementation, graphTests]) {
      const endpoint = moduleByPath(bundle, sourcePath);
      const history = bundle.graph.history_navigation.by_module.items.find(
        (row) => row.module_id === endpoint.id,
      )!;
      history.commits.items = [missingId];
      history.commits.total_count = { status: "available", value: 1 };
      history.commits.next_cursor = null;
      history.commits.truncated = false;
      history.commits.disclosure = { status: "complete" };
    }
    const leftId = moduleByPath(bundle, graphImplementation).id;
    const rightId = moduleByPath(bundle, graphTests).id;
    const pair = bundle.aggregates.hidden_coupling.data.items.find((row) =>
      [row.left_module_id, row.right_module_id].includes(leftId) &&
      [row.left_module_id, row.right_module_id].includes(rightId)
    )!;
    pair.cochange_count = 1;

    const result = comparison(bundle);

    expect(result.status).toBe("available");
    if (result.status !== "available") return;
    expect(result.value.sharedCommits).toMatchObject({
      state: "unknown",
      items: [],
      boundary: {
        status: "truncated",
        shown: 0,
        declared: null,
        reason: expect.stringMatching(/1 shared commit ID.*no captured commit record/i),
      },
    });
  });

  it("propagates truncated commit-record coverage into the shared-commit boundary", () => {
    const bundle = golden();
    bundle.graph.commits.truncated = true;
    bundle.graph.commits.next_cursor = "next-commit-record-page";
    bundle.graph.commits.disclosure = {
      status: "truncated",
      reason: "commit record page reached its bound",
    };

    const result = comparison(bundle);

    expect(result.status).toBe("available");
    if (result.status !== "available") return;
    expect(result.value.sharedCommits.state).toBe("present");
    expect(result.value.sharedCommits.boundary).toMatchObject({
      status: "truncated",
      declared: null,
      reason: expect.stringContaining("commit record page reached its bound"),
    });
  });

  it("does not declare more shared commits than the producer co-change row", () => {
    const bundle = golden();
    const left = moduleByPath(bundle, graphImplementation);
    const right = moduleByPath(bundle, graphTests);
    const template = bundle.graph.commits.items[0];
    for (const index of [0, 1]) {
      const id = `khive:commit:extra-in-window-${index}`;
      bundle.graph.commits.items.push({
        ...template,
        id,
        sha: `${index + 1}`.repeat(40),
        short_sha: `${index + 1}`.repeat(8),
        committed_at: `2026-07-0${index + 1}T00:00:00Z`,
      });
      for (const endpoint of [left, right]) {
        bundle.graph.history_navigation.by_module.items.find((row) =>
          row.module_id === endpoint.id
        )!.commits.items.push(id);
      }
    }

    const result = comparison(bundle);

    expect(result.status).toBe("available");
    if (result.status !== "available") return;
    expect(result.value.sharedCommits.boundary).toMatchObject({
      status: "truncated",
      declared: null,
      reason: expect.stringMatching(/26 captured in-window intersections.*24 producer-declared co-changes/i),
    });
  });

  it("combines module-page coverage and preserves mixed relations for common neighbors", () => {
    const bundle = golden();
    const left = moduleByPath(bundle, graphImplementation);
    const right = moduleByPath(bundle, graphTests);
    const neighbor = moduleByPath(bundle, pool);
    bundle.graph.structure_edges.items = bundle.graph.structure_edges.items
      .filter((edge) =>
        ![left.id, right.id].includes(edge.source) &&
        ![left.id, right.id].includes(edge.target)
      );
    const template = bundle.graph.structure_edges.items[0];
    bundle.graph.structure_edges.items.push(
      {
        ...template,
        id: "mixed-left-neighbor",
        source: left.id,
        target: neighbor.id,
        relation: "depends_on",
      },
      {
        ...template,
        id: "mixed-right-neighbor",
        source: right.id,
        target: neighbor.id,
        relation: "enables",
      },
    );

    const result = comparison(bundle);

    expect(result.status).toBe("available");
    if (result.status !== "available") return;
    expect(result.value.commonNeighbors.items).toHaveLength(1);
    expect(result.value.commonNeighbors.items[0]).toMatchObject({
      module: { source_path: pool },
      leftDirection: "outgoing",
      leftRelation: "depends_on",
      rightDirection: "outgoing",
      rightRelation: "enables",
    });
  });

  it("keeps common-neighbor relation work near-linear before the six-row output cap", () => {
    const bundle = golden();
    const left = moduleByPath(bundle, graphImplementation);
    const right = moduleByPath(bundle, graphTests);
    const neighbor = moduleByPath(bundle, pool);
    bundle.graph.structure_edges.items = bundle.graph.structure_edges.items
      .filter((edge) =>
        ![left.id, right.id].includes(edge.source) &&
        ![left.id, right.id].includes(edge.target)
      );
    const template = bundle.graph.structure_edges.items[0];
    let relationCoercions = 0;
    const countedRelation = (label: string) => ({
      [Symbol.toPrimitive]() {
        relationCoercions += 1;
        return label;
      },
    }) as unknown as string;
    for (let index = 0; index < 120; index += 1) {
      bundle.graph.structure_edges.items.push(
        {
          ...template,
          id: `bounded-left-${index}`,
          source: left.id,
          target: neighbor.id,
          relation: countedRelation(`left-${index}`),
        },
        {
          ...template,
          id: `bounded-right-${index}`,
          source: right.id,
          target: neighbor.id,
          relation: countedRelation(`right-${index}`),
        },
      );
    }
    bundle.graph.structure_edges.total_count = {
      status: "available",
      value: bundle.graph.structure_edges.items.length,
    };

    const result = comparison(bundle);

    expect(result.status).toBe("available");
    if (result.status !== "available") return;
    expect(result.value.commonNeighbors.items).toHaveLength(
      COUPLING_COMMON_NEIGHBOR_LIMIT,
    );
    expect(result.value.commonNeighbors.boundary).toMatchObject({
      status: "truncated",
      shown: COUPLING_COMMON_NEIGHBOR_LIMIT,
      declared: 14_400,
      bound: COUPLING_COMMON_NEIGHBOR_LIMIT,
      reason: expect.stringMatching(/14394 additional captured common neighbors/i),
    });
    expect(relationCoercions).toBeLessThanOrEqual(300);
  });

  it("does not turn an unresolved neighbor into absence when the module page is truncated", () => {
    const bundle = golden();
    const left = moduleByPath(bundle, graphImplementation);
    const right = moduleByPath(bundle, graphTests);
    bundle.graph.structure_edges.items = bundle.graph.structure_edges.items
      .filter((edge) =>
        ![left.id, right.id].includes(edge.source) &&
        ![left.id, right.id].includes(edge.target)
      );
    const template = bundle.graph.structure_edges.items[0];
    bundle.graph.structure_edges.items.push(
      { ...template, id: "missing-left", source: left.id, target: "uncaptured-neighbor", relation: "depends_on" },
      { ...template, id: "missing-right", source: right.id, target: "uncaptured-neighbor", relation: "depends_on" },
    );
    bundle.graph.modules.truncated = true;
    bundle.graph.modules.next_cursor = "next-module-page";
    bundle.graph.modules.disclosure = {
      status: "truncated",
      reason: "module page reached its bound",
    };

    const result = comparison(bundle);

    expect(result.status).toBe("available");
    if (result.status !== "available") return;
    expect(result.value.commonNeighbors).toMatchObject({
      state: "unknown",
      items: [],
      boundary: {
        status: "truncated",
        declared: null,
        reason: expect.stringMatching(/module page reached its bound.*1 captured common-neighbor ID.*outside/i),
      },
    });
  });

  it("fails closed when a complete module page omits a referenced common neighbor", () => {
    const bundle = golden();
    const left = moduleByPath(bundle, graphImplementation);
    const right = moduleByPath(bundle, graphTests);
    const template = bundle.graph.structure_edges.items[0];
    bundle.graph.structure_edges.items.push(
      { ...template, id: "missing-complete-left", source: left.id, target: "missing-complete-neighbor", relation: "depends_on" },
      { ...template, id: "missing-complete-right", source: right.id, target: "missing-complete-neighbor", relation: "depends_on" },
    );

    expect(comparison(bundle)).toMatchObject({
      status: "unavailable",
      code: "referenced_module_missing",
      reason: expect.stringContaining("missing-complete-neighbor"),
    });
  });

  it("caps members inside each SCC and discloses every omitted captured member", () => {
    const bundle = golden();
    const endpoint = moduleByPath(bundle, graphImplementation);
    const members = [
      endpoint.id,
      ...bundle.graph.modules.items
        .filter((moduleNode) => moduleNode.id !== endpoint.id)
        .slice(0, 11)
        .map((moduleNode) => moduleNode.id),
    ];
    const topology = bundle.aggregates.dependency_topology.modules.items.find(
      (row) => row.module_id === endpoint.id,
    )!;
    topology.cycle_ids = ["large-focused-cycle"];
    bundle.aggregates.dependency_topology.cycles.items.push({
      id: "large-focused-cycle",
      module_ids: members,
    });
    if (bundle.aggregates.dependency_topology.cycles.total_count.status === "available") {
      bundle.aggregates.dependency_topology.cycles.total_count.value += 1;
    }

    const result = comparison(bundle);

    expect(result.status).toBe("available");
    if (result.status !== "available") return;
    const cycle = result.value.endpoints[0].scc.items[0];
    expect(cycle.modules).toHaveLength(6);
    expect(cycle.memberBoundary).toMatchObject({
      status: "truncated",
      shown: 6,
      declared: 12,
      bound: 6,
      reason: expect.stringMatching(/6 additional captured SCC members.*fixed display bound/i),
    });
  });

  it("discloses both upstream truncation and local cap omissions when the declared total is unknown", () => {
    const bundle = golden();
    const left = moduleByPath(bundle, graphImplementation);
    const right = moduleByPath(bundle, graphTests);
    const leftHistory = bundle.graph.history_navigation.by_module.items.find(
      (row) => row.module_id === left.id,
    )!;
    const rightIds = new Set(
      bundle.graph.history_navigation.by_module.items.find((row) =>
        row.module_id === right.id
      )!.commits.items,
    );
    leftHistory.commits.items = leftHistory.commits.items
      .filter((id) => rightIds.has(id))
      .slice(0, 12);
    leftHistory.commits.truncated = true;
    leftHistory.commits.next_cursor = "next-history-page";
    leftHistory.commits.disclosure = {
      status: "truncated",
      reason: "history source is incomplete",
    };

    const result = comparison(bundle);

    expect(result.status).toBe("available");
    if (result.status !== "available") return;
    expect(result.value.sharedCommits.boundary).toMatchObject({
      status: "truncated",
      shown: 5,
      declared: null,
      bound: 5,
      reason: expect.stringMatching(/history source is incomplete.*7 additional shared captured commits.*fixed display bound/i),
    });
  });

  it("counts only locally omitted captured author rows when the producer total is larger", () => {
    const bundle = golden();
    const endpoint = moduleByPath(bundle, graphImplementation);
    const ownership = bundle.aggregates.ownership.modules.items.find((row) =>
      row.module_id === endpoint.id
    )!;
    ownership.authors.items = Array.from({ length: 50 }, (_, index) => ({
      author: `captured-author-${index}`,
      commits: 50 - index,
      share: 0.02,
    }));
    ownership.authors.total_count = { status: "available", value: 100 };

    const result = comparison(bundle);

    expect(result.status).toBe("available");
    if (result.status !== "available") return;
    expect(result.value.endpoints[0].ownership.boundary.reason).toMatch(
      /47 additional captured author rows.*fixed display bound/i,
    );
    expect(result.value.endpoints[0].ownership.boundary.reason).not.toMatch(
      /97 additional/i,
    );
  });

  it("separates missing SCC records from locally omitted captured SCC rows", () => {
    const bundle = golden();
    const endpoint = moduleByPath(bundle, graphImplementation);
    const capturedCycleIds = bundle.aggregates.dependency_topology.cycles.items
      .slice(0, 4)
      .map((cycle) => cycle.id);
    bundle.aggregates.dependency_topology.modules.items.find((row) =>
      row.module_id === endpoint.id
    )!.cycle_ids = [...capturedCycleIds, "missing-focused-cycle"];
    bundle.aggregates.dependency_topology.cycles.truncated = true;
    bundle.aggregates.dependency_topology.cycles.next_cursor = "next-cycle-page";
    bundle.aggregates.dependency_topology.cycles.disclosure = {
      status: "truncated",
      reason: "cycle page reached its bound",
    };

    const result = comparison(bundle);

    expect(result.status).toBe("available");
    if (result.status !== "available") return;
    expect(result.value.endpoints[0].scc.boundary).toMatchObject({
      shown: 3,
      declared: 5,
      status: "truncated",
      reason: expect.stringMatching(
        /1 declared SCC record is outside.*1 additional captured SCC record.*fixed display bound/i,
      ),
    });
    expect(result.value.endpoints[0].scc.boundary.reason).not.toMatch(
      /2 additional captured SCC/i,
    );
  });

  it("does not turn a truncated module page into an unavailable comparison when an SCC member is outside it", () => {
    const bundle = golden();
    const endpoint = moduleByPath(bundle, graphImplementation);
    bundle.aggregates.dependency_topology.modules.items.find((row) =>
      row.module_id === endpoint.id
    )!.cycle_ids = ["focused-cycle-with-uncaptured-member"];
    bundle.aggregates.dependency_topology.cycles.items.push({
      id: "focused-cycle-with-uncaptured-member",
      module_ids: [endpoint.id, "uncaptured-module"],
    });
    if (
      bundle.aggregates.dependency_topology.cycles.total_count.status ===
        "available"
    ) {
      bundle.aggregates.dependency_topology.cycles.total_count.value += 1;
    }
    bundle.graph.modules.truncated = true;
    bundle.graph.modules.next_cursor = "next-module-page";
    bundle.graph.modules.disclosure = {
      status: "truncated",
      reason: "module page reached its bound",
    };

    const result = comparison(bundle);

    expect(result.status).toBe("available");
    if (result.status !== "available") return;
    expect(result.value.endpoints[0].scc).toMatchObject({
      state: "present",
      items: [],
      boundary: {
        status: "truncated",
        reason: expect.stringMatching(/module page reached its bound/i),
      },
    });
  });

  it("still fails closed when a complete module page omits a referenced SCC member", () => {
    const bundle = golden();
    const endpoint = moduleByPath(bundle, graphImplementation);
    bundle.aggregates.dependency_topology.modules.items.find((row) =>
      row.module_id === endpoint.id
    )!.cycle_ids = ["focused-cycle-with-missing-complete-member"];
    bundle.aggregates.dependency_topology.cycles.items.push({
      id: "focused-cycle-with-missing-complete-member",
      module_ids: [endpoint.id, "missing-complete-module"],
    });
    if (
      bundle.aggregates.dependency_topology.cycles.total_count.status ===
        "available"
    ) {
      bundle.aggregates.dependency_topology.cycles.total_count.value += 1;
    }

    expect(comparison(bundle)).toMatchObject({
      status: "unavailable",
      code: "referenced_module_missing",
      reason: expect.stringContaining("missing-complete-module"),
    });
  });

  it("makes shared commits unknown when complete evidence contradicts the producer count", () => {
    const bundle = golden();
    const left = moduleByPath(bundle, graphImplementation);
    const right = moduleByPath(bundle, graphTests);
    const rightHistory = bundle.graph.history_navigation.by_module.items.find(
      (row) => row.module_id === right.id,
    )!;
    const leftHistory = bundle.graph.history_navigation.by_module.items.find(
      (row) => row.module_id === left.id,
    )!;
    const removedSharedId = rightHistory.commits.items.find((id) =>
      leftHistory.commits.items.includes(id)
    )!;
    leftHistory.commits.items = leftHistory.commits.items.filter((id) =>
      id !== removedSharedId
    );
    leftHistory.commits.total_count = {
      status: "available",
      value: leftHistory.commits.items.length,
    };

    const result = comparison(bundle);

    expect(result.status).toBe("available");
    if (result.status !== "available") return;
    expect(result.value.sharedCommits).toMatchObject({
      state: "unknown",
      items: [],
      boundary: {
        status: "truncated",
        shown: 0,
        declared: null,
        reason: expect.stringMatching(
          /23 captured in-window intersections do not equal 24 producer-declared co-changes/i,
        ),
      },
    });
  });
});
