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

    expect(brief.metrics.modules.shown).toBe(bundle.graph.modules.items.length);
    expect(brief.metrics.modules.status).toBe("complete");
    expect(brief.metrics.commits.shown).toBe(bundle.graph.commits.items.length);
    expect(brief.metrics.cycles.shown).toBe(
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

  it("preserves unavailable and truncated page semantics in summary metrics", () => {
    const partial = structuredClone(bundle);
    partial.capability.labels.truncated = "Contract partial";
    partial.capability.labels.unavailable = "Contract unavailable";
    partial.graph.modules.items = partial.graph.modules.items.slice(0, 2);
    partial.graph.modules.total_count = { status: "available", value: 37 };
    partial.graph.modules.truncated = true;
    partial.graph.modules.disclosure = {
      status: "truncated",
      reason: "module export was capped",
    };
    partial.graph.commits.items = [];
    partial.graph.commits.total_count = {
      status: "unavailable",
      reason: "history count was not measured",
    };
    partial.graph.commits.disclosure = {
      status: "unavailable",
      reason: "history was not measured",
    };

    const brief = buildRepositoryBrief(partial);

    expect(brief.metrics.modules).toMatchObject({
      shown: 2,
      total: 37,
      status: "truncated",
    });
    expect(brief.metrics.modules.summary).toMatch(/2 captured of 37; Contract partial/i);
    expect(brief.metrics.commits).toMatchObject({
      shown: 0,
      total: null,
      status: "unavailable",
    });
    expect(brief.metrics.commits.summary).toMatch(/total Contract unavailable/i);
  });

  it("keeps a complete captured page complete when only its total is unknown", () => {
    const unknownTotal = structuredClone(bundle);
    unknownTotal.graph.commits.total_count = {
      status: "unavailable",
      reason: "the producer did not count all commits",
    };
    unknownTotal.graph.commits.truncated = false;
    unknownTotal.graph.commits.next_cursor = null;
    unknownTotal.graph.commits.disclosure = { status: "complete", reason: null };

    const metric = buildRepositoryBrief(unknownTotal).metrics.commits;

    expect(metric).toMatchObject({
      shown: unknownTotal.graph.commits.items.length,
      total: null,
      status: "complete",
    });
    expect(metric.summary).toMatch(/total unavailable/i);
  });

  it("keeps unavailable recommendation analyses distinct from measured empty", () => {
    const unavailable = structuredClone(bundle);
    unavailable.aggregates.api_surface.meta.status = "unavailable";
    unavailable.aggregates.api_surface.meta.unavailable_reason = "API ranking was not produced";
    for (const analysis of [
      unavailable.aggregates.hotspot_quadrant,
      unavailable.aggregates.dependency_topology,
      unavailable.aggregates.hidden_coupling,
      unavailable.aggregates.ownership,
    ]) {
      analysis.meta.status = "unavailable";
      analysis.meta.unavailable_reason = "attention analysis was not produced";
    }

    const brief = buildRepositoryBrief(unavailable);

    expect(brief.startHere).toEqual([]);
    expect(brief.startHereState).toMatchObject({
      status: "unavailable",
      reason: "API ranking was not produced",
    });
    expect(brief.attentionSignals).toEqual([]);
    expect(brief.attentionState).toMatchObject({ status: "unavailable" });
    expect(brief.attentionState.reason).toContain("attention analysis was not produced");
    expect(brief.metrics.cycles).toMatchObject({
      status: "unavailable",
      reason: "attention analysis was not produced",
    });
  });

  it("discloses one unavailable attention analysis without hiding other signals", () => {
    const partial = structuredClone(bundle);
    partial.aggregates.hidden_coupling.meta.status = "unavailable";
    partial.aggregates.hidden_coupling.meta.unavailable_reason =
      "coupling analysis was not produced";

    const brief = buildRepositoryBrief(partial);

    expect(brief.attentionSignals.length).toBeGreaterThan(0);
    expect(
      brief.attentionSignals.some((signal) => signal.kind === "hidden_coupling"),
    ).toBe(false);
    expect(brief.attentionState).toMatchObject({
      status: "unavailable",
      reason: "coupling analysis was not produced",
    });
  });

  it("does not fabricate recommendations from zero-evidence rows or cycle order", () => {
    const quiet = structuredClone(bundle);
    quiet.aggregates.hotspot_quadrant.data.items = quiet.aggregates
      .hotspot_quadrant.data.items.map((row) => ({
        ...row,
        commit_count: 0,
        fan_in: 0,
        quadrant: "low_churn_low_fan_in" as const,
      }));
    quiet.aggregates.api_surface.data.items = quiet.aggregates.api_surface.data
      .items.map((row) => ({
        ...row,
        dependent_count: 0,
      }));

    const brief = buildRepositoryBrief(quiet);
    const cycle = brief.attentionSignals.find(
      (signal) => signal.kind === "dependency_cycle",
    );

    expect(brief.startHere).toEqual([]);
    expect(
      brief.attentionSignals.some((signal) => signal.kind === "hotspot"),
    ).toBe(false);
    expect(cycle?.summary).toMatch(/SCC members:/i);
    expect(cycle?.summary).not.toContain("→");
  });

  it("builds one module inspector from topology, history, ownership, and coupling evidence", () => {
    const target = buildRepositoryBrief(bundle).startHere[0];
    const insight = buildModuleInsight(bundle, target.moduleId);

    expect(insight).not.toBeNull();
    expect(insight?.module.source_path).toBe(target.sourcePath);
    expect(insight?.topology.fanIn).toBe(target.dependentCount);
    expect(insight?.recentCommits.length).toBeGreaterThan(0);
    expect(insight?.history.shown).toBeGreaterThanOrEqual(insight?.recentCommits.length ?? 0);
    expect(insight?.history.total).not.toBeNull();
    expect(insight?.recentCommits[0]).toMatchObject({
      sha: expect.stringMatching(/^[0-9a-f]{40}$/),
    });
    expect(
      insight?.evidence.some((item) => item.label === "Analysis window"),
    ).toBe(true);
    expect(
      insight?.evidence.some(
        (item) =>
          item.label ===
            `${bundle.capability.views.hidden_coupling.label} coverage`,
      ),
    ).toBe(true);
    expect(
      insight?.evidence.some((item) => item.value === "Not classified in this snapshot"),
    ).toBe(true);
  });

  it("does not consume aggregate rows whose analysis is unavailable", () => {
    const unavailable = structuredClone(bundle);
    const target = buildRepositoryBrief(bundle).startHere[0];
    for (const analysis of [
      unavailable.aggregates.hotspot_quadrant,
      unavailable.aggregates.dependency_topology,
      unavailable.aggregates.hidden_coupling,
      unavailable.aggregates.ownership,
      unavailable.aggregates.api_surface,
    ]) {
      analysis.meta.status = "unavailable";
      analysis.meta.unavailable_reason = "not produced";
    }
    unavailable.graph.structure_edges.disclosure.status = "unavailable";
    unavailable.graph.structure_edges.disclosure.reason = "not produced";

    expect(buildModuleInsight(unavailable, target.moduleId)).toMatchObject({
      hotspot: null,
      ownership: null,
      apiSurface: null,
      couplings: [],
      couplingState: { status: "unavailable", reason: "not produced" },
      topology: {
        coverage: { status: "unavailable", reason: "not produced" },
        cycles: [],
        cycleIds: [],
      },
    });
  });

  it("finds a module by the path a user already knows", () => {
    const matches = findRepositoryModules(bundle, "pool.rs", 8);
    expect(matches.items.length).toBeGreaterThan(0);
    expect(matches.items[0].source_path).toContain("pool.rs");
    expect(matches.total).toBe(matches.items.length);
    expect(matches.bound).toBe(8);
  });

  it("reports the total match count when a search is capped by the limit", () => {
    const matches = findRepositoryModules(bundle, "e", 1);
    expect(matches.items.length).toBe(1);
    expect(matches.total).toBeGreaterThan(1);
    expect(matches.bound).toBe(1);
  });
});
