import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { describe, expect, it } from "vitest";

import { parseRepoBundle } from "@/lib/repo-bundle";
import {
  buildModuleInsight,
  buildRepositoryBrief,
  findRepositoryModules,
} from "@/lib/repository-brief";

const bundle = parseRepoBundle(
  JSON.parse(
    readFileSync(
      resolve(
        process.cwd(),
        "../../docs/schemas/examples/khive-repo-v1-khive.json",
      ),
      "utf8",
    ),
  ),
);

describe("repository triage model", () => {
  it("turns the flat analyses into a deterministic evidence-backed brief", () => {
    const apiSurfaceBefore = structuredClone(
      bundle.aggregates.api_surface.data.items,
    );
    const brief = buildRepositoryBrief(bundle);

    expect(brief.metrics.modules).toBe(bundle.graph.modules.items.length);
    expect(brief.metrics.commits).toBe(bundle.graph.commits.items.length);
    expect(brief.metrics.cycles).toBe(
      bundle.aggregates.dependency_topology.cycles.items.length,
    );
    expect(
      brief.attentionSignals.slice(0, 3).map((signal) => signal.kind),
    ).toEqual(["hotspot", "dependency_cycle", "hidden_coupling"]);
    expect(
      brief.attentionSignals.find(
        (signal) => signal.kind === "dependency_cycle",
      ),
    ).toMatchObject({ classification: "observed" });
    expect(
      brief.attentionSignals
        .filter((signal) => signal.kind !== "dependency_cycle")
        .every((signal) => signal.classification === "candidate"),
    ).toBe(true);
    expect(brief.startHere).toHaveLength(3);
    expect(brief.startHere.map((entry) => entry.dependentCount)).toEqual(
      [...brief.startHere.map((entry) => entry.dependentCount)].sort(
        (left, right) => right - left,
      ),
    );
    expect(
      brief.attentionSignals.every((signal) => signal.evidence.length > 0),
    ).toBe(true);
    expect(bundle.aggregates.api_surface.data.items).toEqual(apiSurfaceBefore);
  });

  it("builds one module inspector from topology, history, ownership, and coupling evidence", () => {
    const target = buildRepositoryBrief(bundle).startHere[0];
    const insight = buildModuleInsight(bundle, target.moduleId);

    expect(insight).not.toBeNull();
    expect(insight?.module.source_path).toBe(target.sourcePath);
    expect(insight?.topology.fanIn).toBe(target.dependentCount);
    expect(insight?.recentCommits.length).toBeGreaterThan(0);
    expect(insight?.recentCommits[0]).toMatchObject({
      sha: expect.stringMatching(/^[0-9a-f]{40}$/),
    });
    expect(
      insight?.evidence.some((item) => item.label === "Analysis window"),
    ).toBe(true);
  });

  it("finds a module by the path a user already knows", () => {
    const hits = findRepositoryModules(bundle, "pool.rs", 8);
    expect(hits.length).toBeGreaterThan(0);
    expect(hits[0].source_path).toContain("pool.rs");
  });
});
