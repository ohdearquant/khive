import { describe, expect, it } from "vitest";

import { demoReviewFixture } from "@/lib/fixtures/demo-review";
import type { ReviewReport } from "@/lib/review-bundle";
import {
  buildReviewThreadModel,
  reviewUnitKey,
  type ReviewAnnotation,
} from "@/lib/review-thread";

const conceptId = "a1f00000-0000-4000-8000-000000000001";
const sourceId = "a1f00000-0000-4000-8000-000000000002";
const enrichedOnlyId = "a1f00000-0000-4000-8000-000000000099";

function changesetReport(): ReviewReport {
  const { change_set, tier_summary, validation, findings, review_gate } = demoReviewFixture;
  return {
    schema_version: "khive.review.v1",
    review_kind: "changeset",
    capability: {
      source: "local_changeset",
      mutability: "read_only",
      no_writes: true,
      git_reads: false,
      khive_reads: false,
      github_writes: false,
      wasm: false,
      persistence: false,
      unavailable_actions: ["stage", "apply", "commit", "push", "publish"],
    },
    change_set: { ...change_set, operations: [...change_set.operations] },
    tier_summary: { ...tier_summary },
    validation: { ...validation },
    findings: [...findings],
    review_gate: { ...review_gate },
  };
}

describe("review thread model", () => {
  it("orders a selected PR unit by source time, not fixture insertion order", () => {
    // The fixture inserts evidence-prov-o before evidence-incident, although
    // the incident was captured first; the operation was staged later still.
    expect(demoReviewFixture.evidence.items.map((item) => item.id)).toEqual([
      "evidence-prov-o",
      "evidence-incident",
    ]);
    const model = buildReviewThreadModel(demoReviewFixture);
    const unit = model.units.find((row) => row.key === reviewUnitKey("entity", conceptId));

    expect(unit).toBeDefined();
    expect(unit?.events.map((event) => event.key)).toEqual([
      "evidence:evidence-incident",
      "evidence:evidence-prov-o",
      `operation:0:${conceptId}`,
    ]);
    expect(unit?.events.at(-1)).toMatchObject({ timeSource: "batch_staged_at" });
    expect(unit?.evidence).toMatchObject({
      status: "available",
      linkedIds: ["evidence-prov-o", "evidence-incident"],
      unresolvedIds: [],
    });
    expect(model.units).toHaveLength(demoReviewFixture.change_set.operations.length);
    expect(new Set(model.units.map((row) => row.key)).size).toBe(model.units.length);
    expect(model.activityStatus).toBe("unlinked");
    expect(model.unlinkedActivity).toHaveLength(demoReviewFixture.activity.items.length);
    expect(unit?.events.some((event) => event.kind === "annotation")).toBe(false);
  });

  it("uses the same unit/thread shape for a core changeset and preserves operation indexes at staged_at", () => {
    const first = demoReviewFixture.change_set.operations[0];
    const report = changesetReport();
    report.change_set.operations = [
      { ...first, index: 2, summary: "Third operation" },
      { ...first, index: 0, summary: "First operation" },
      { ...first, index: 1, summary: "Second operation" },
    ];
    report.tier_summary.operations = 3;
    report.tier_summary.tier_1 = 3;
    report.tier_summary.tier_2 = 0;
    report.tier_summary.highest_tier = "tier_1";

    const model = buildReviewThreadModel(report);
    expect(model.units).toHaveLength(1);
    expect(model.units[0]).toMatchObject({
      key: reviewUnitKey("entity", conceptId),
      id: conceptId,
      substrate: "entity",
      evidence: { status: "unavailable" },
    });
    expect(model.units[0].events.map((event) =>
      event.kind === "operation" ? event.operation.index : null,
    )).toEqual([0, 1, 2]);
    expect(model.activityStatus).toBe("unavailable");

    const enrichedOnly = {
      ...demoReviewFixture.changes.items[0],
      id: enrichedOnlyId,
      title: "Enrichment-only review unit",
      evidence_ids: ["evidence-prov-o"],
    };
    const prInput = {
      ...demoReviewFixture,
      changes: {
        ...demoReviewFixture.changes,
        items: [...demoReviewFixture.changes.items, enrichedOnly],
      },
      summary: {
        ...demoReviewFixture.summary,
        entities_added: demoReviewFixture.summary.entities_added + 1,
      },
    };
    const pr = buildReviewThreadModel(prInput);
    expect(pr.units.find((unit) => unit.key === reviewUnitKey("entity", conceptId))).toMatchObject({
      key: model.units[0].key,
      id: model.units[0].id,
      substrate: model.units[0].substrate,
      evidence: { status: "available" },
    });
    expect(pr.units.find((unit) => unit.key === reviewUnitKey("entity", conceptId))?.events
      .some((event) => event.kind === "evidence")).toBe(true);
    expect(pr.units.find((unit) => unit.key === reviewUnitKey("entity", enrichedOnlyId)))
      .toMatchObject({
        id: enrichedOnlyId,
        substrate: "entity",
        operations: [],
        evidence: { status: "available" },
      });
    expect(pr.units.find((unit) => unit.key === reviewUnitKey("entity", enrichedOnlyId))?.events
      .map((event) => event.key)).toEqual(["evidence:evidence-prov-o"]);
  });

  it("keeps findings undated and admits only explicitly unit-linked annotations", () => {
    const conceptKey = reviewUnitKey("entity", conceptId);
    const annotations: ReviewAnnotation[] = [
      {
        id: "later",
        unitKey: conceptKey,
        actor: "actor:reviewer",
        body: "Follow-up",
        createdAt: "2026-08-07T18:00:00Z",
      },
      {
        id: "earlier",
        unitKey: conceptKey,
        actor: "actor:reviewer",
        body: "Initial note",
        createdAt: "2026-08-07T15:00:00Z",
      },
      {
        id: "orphan",
        unitKey: reviewUnitKey("note", "absent"),
        actor: "actor:reviewer",
        body: "No matching unit",
        createdAt: "2026-08-07T15:30:00Z",
      },
    ];
    const model = buildReviewThreadModel(demoReviewFixture, annotations);
    const concept = model.units.find((unit) => unit.key === conceptKey);
    expect(concept?.events.map((event) => event.key)).toEqual([
      "evidence:evidence-incident",
      "annotation:earlier",
      "evidence:evidence-prov-o",
      `operation:0:${conceptId}`,
      "annotation:later",
    ]);
    expect(model.unlinkedAnnotations.map((annotation) => annotation.id)).toEqual(["orphan"]);

    const source = model.units.find((unit) => unit.key === reviewUnitKey("entity", sourceId));
    const finding = source?.events.at(-1);
    expect(finding).toMatchObject({
      kind: "finding",
      occurredAt: null,
      occurredAtUs: null,
      timeSource: "unavailable",
    });
    expect(model.unlinkedActivity).toHaveLength(3);
  });

  it("distinguishes no evidence link, unavailable enrichment, and incomplete or missing pages", () => {
    const withoutLink = {
      ...demoReviewFixture,
      changes: {
        ...demoReviewFixture.changes,
        items: demoReviewFixture.changes.items.map((change) =>
          change.id === conceptId ? { ...change, evidence_ids: [] } : change,
        ),
      },
    };
    expect(buildReviewThreadModel(withoutLink).units[0].evidence.status).toBe("unlinked");

    const unavailable = {
      ...demoReviewFixture,
      enrichment_status: { ...demoReviewFixture.enrichment_status, evidence: "unavailable" as const },
      evidence: { ...demoReviewFixture.evidence, items: [] },
    };
    expect(buildReviewThreadModel(unavailable).units[0].evidence).toMatchObject({
      status: "unavailable",
      unresolvedIds: ["evidence-prov-o", "evidence-incident"],
    });

    const partial = {
      ...demoReviewFixture,
      evidence: {
        ...demoReviewFixture.evidence,
        items: demoReviewFixture.evidence.items.slice(0, 1),
        truncated: true,
      },
    };
    expect(buildReviewThreadModel(partial).units[0].evidence).toMatchObject({
      status: "truncated",
      unresolvedIds: ["evidence-incident"],
    });
    const completeButMissing = {
      ...partial,
      evidence: { ...partial.evidence, truncated: false },
    };
    expect(buildReviewThreadModel(completeButMissing).units[0].evidence.status).toBe("missing");

    const extraEvidence = {
      ...demoReviewFixture,
      evidence: {
        ...demoReviewFixture.evidence,
        items: [
          ...demoReviewFixture.evidence.items,
          { ...demoReviewFixture.evidence.items[0], id: "unassigned-evidence" },
        ],
      },
    };
    expect(buildReviewThreadModel(extraEvidence).unassignedEvidence.map((item) => item.id))
      .toEqual(["unassigned-evidence"]);
  });
});
