import { describe, expect, it } from "vitest";

import { demoReviewFixture } from "@/lib/fixtures/demo-review";
import {
  isReviewReport,
  parseReviewBundle,
  parseReviewInput,
  REVIEW_CORE_MAX_ITEMS,
  REVIEW_PAGE_MAX_ITEMS,
} from "@/lib/review-bundle";
import {
  canApproveReview,
  groupChanges,
  matchesReviewQuery,
  shortHash,
} from "@/lib/review-utils";

describe("khive.review.v1", () => {
  it("keeps present identities distinct and marks an absent live proposal explicitly", () => {
    const bundle = parseReviewBundle(demoReviewFixture);

    expect(bundle.repository.head_sha).toHaveLength(40);
    expect(bundle.pull_request.head_sha).toBe(bundle.repository.head_sha);
    expect(bundle.snapshot_identity.hash_status).toBe("fixture");
    expect(bundle.snapshot_identity.head_hash).toMatch(/^sha256:/);
    expect(bundle.pull_request.number).toBe(184);
    expect(bundle.change_set.envelope.batch_id).toBe("demo-enrich-2026-08-07-184");
    expect(bundle.change_set.envelope.producer).toBe("actor:casey");
    expect(bundle.live_proposal).toBeNull();
  });

  it("fails closed when the version or a full identifier is missing", () => {
    expect(() =>
      parseReviewBundle({ ...demoReviewFixture, schema_version: "khive.review.v2" }),
    ).toThrow();
    expect(() =>
      parseReviewBundle({
        ...demoReviewFixture,
        repository: { ...demoReviewFixture.repository, head_sha: undefined },
      }),
    ).toThrow();
  });

  it("represents a stale pull-request head without rejecting the review bundle", () => {
    const stale = parseReviewBundle({
      ...demoReviewFixture,
      pull_request: { ...demoReviewFixture.pull_request, head_sha: "0".repeat(40) },
    });

    expect(stale.pull_request.head_sha).not.toBe(stale.repository.head_sha);
    expect(canApproveReview(stale, "family:independent-reasoner")).toEqual({
      allowed: false,
      reason: "The pull-request head changed; regenerate the semantic review bundle.",
    });
  });

  it("does not accept verified canonical hashes before an algorithm is ratified", () => {
    expect(() =>
      parseReviewBundle({
        ...demoReviewFixture,
        snapshot_identity: {
          ...demoReviewFixture.snapshot_identity,
          hash_status: "verified",
        },
      }),
    ).toThrow();
  });

  it("bounds adapter pages and in-memory operation lists", () => {
    const firstCheck = demoReviewFixture.checks.items[0];
    expect(() =>
      parseReviewBundle({
        ...demoReviewFixture,
        checks: {
          ...demoReviewFixture.checks,
          items: Array.from({ length: REVIEW_PAGE_MAX_ITEMS + 1 }, (_, index) => ({
            ...firstCheck,
            id: `bounded-check-${index}`,
          })),
        },
      }),
    ).toThrow();

    const firstOperation = demoReviewFixture.change_set.operations[0];
    expect(() =>
      parseReviewBundle({
        ...demoReviewFixture,
        change_set: {
          ...demoReviewFixture.change_set,
          operations: Array.from({ length: REVIEW_CORE_MAX_ITEMS + 1 }, (_, index) => ({
            ...firstOperation,
            index,
            id: `bounded-operation-${index}`,
          })),
        },
      }),
    ).toThrow();
  });

  it("parses the minimal changeset report emitted by the Rust CLI", () => {
    const value = parseReviewInput({
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
      change_set: {
        envelope: {
          schema_version: 1,
          producer: "actor:casey",
          producer_model_family: "family:demo",
          staged_at: 2_000_000,
          batch_id: "demo-batch-7",
        },
        operations: [
          {
            index: 0,
            id: "10000000-0000-4000-8000-000000000001",
            op: "create",
            target: "entity",
            tier: "tier_1",
            summary: "Create entity Alpha",
            reason: "Additive entity creation is eligible for the fast path.",
            tier_reasons: ["operation.create"],
            entity_ids: ["10000000-0000-4000-8000-000000000001"],
            after: { kind: "concept", name: "Alpha" },
          },
        ],
      },
      tier_summary: {
        operations: 1,
        tier_1: 1,
        tier_2: 0,
        highest_tier: "tier_1",
        requires_independent_review: false,
        policy: "adr_102_floor",
      },
      validation: {
        scope: "commit_time_partial_view",
        rules_evaluated: 0,
        failed_rules: 0,
        errors: 0,
        warnings: 0,
        info: 0,
        passed: true,
      },
      findings: [],
      review_gate: {
        required: false,
        producer_model_family: "family:demo",
        reviewer_model_family: null,
        eligible: true,
        approval_ready: true,
        status: "not_required",
        reason: "No operation requires independent Tier 2 review.",
        persisted: false,
      },
    });

    expect(isReviewReport(value)).toBe(true);
    if (isReviewReport(value)) {
      expect(value.validation.scope).toBe("commit_time_partial_view");
      expect(value.capability.github_writes).toBe(false);
    }
  });
});

describe("review gate", () => {
  it("refuses a same-family reviewer", () => {
    expect(
      canApproveReview(
        demoReviewFixture,
        demoReviewFixture.change_set.envelope.producer_model_family,
      ),
    ).toEqual({
      allowed: false,
      reason: "ADR-102 requires a reviewer outside family:demo-frontier.",
    });
  });

  it("allows an independent reviewer when no required check failed", () => {
    expect(canApproveReview(demoReviewFixture, "family:independent-reasoner").allowed).toBe(
      true,
    );
  });

  it("refuses approval when any required check fails", () => {
    const bundle = {
      ...demoReviewFixture,
      checks: {
        ...demoReviewFixture.checks,
        items: demoReviewFixture.checks.items.map((check, index) =>
          index === 0 ? { ...check, status: "fail" as const } : check,
        ),
      },
    };

    expect(canApproveReview(bundle, "family:independent-reasoner")).toEqual({
      allowed: false,
      reason: "1 required check failed.",
    });
  });

  it("blocks non-reviewer pending checks but resolves the fixture's reviewer check locally", () => {
    expect(canApproveReview(demoReviewFixture, "family:independent-reasoner").allowed).toBe(
      true,
    );

    const pending = {
      ...demoReviewFixture,
      checks: {
        ...demoReviewFixture.checks,
        items: [
          ...demoReviewFixture.checks.items,
          {
            id: "github-required-check",
            label: "Repository policy",
            status: "pending" as const,
            detail: "Waiting for the repository policy check.",
            duration_ms: 0,
          },
        ],
      },
    };

    expect(canApproveReview(pending, "family:independent-reasoner")).toEqual({
      allowed: false,
      reason: "1 required check is still pending.",
    });
  });

  it("respects semantic gate blockers that a local reviewer selection cannot resolve", () => {
    const blocked = {
      ...demoReviewFixture,
      review_gate: {
        ...demoReviewFixture.review_gate,
        eligible: true,
        approval_ready: false,
        status: "blocked_by_repository_policy",
        reason: "Repository policy has not admitted this semantic review.",
      },
    };

    expect(canApproveReview(blocked, "family:independent-reasoner")).toEqual({
      allowed: false,
      reason: "Repository policy has not admitted this semantic review.",
    });
  });

  it("refuses stale PR heads and error-level semantic findings", () => {
    const stale = parseReviewBundle({
      ...demoReviewFixture,
      pull_request: { ...demoReviewFixture.pull_request, head_sha: "0".repeat(40) },
    });
    expect(canApproveReview(stale, "family:independent-reasoner").reason).toMatch(
      /head changed/i,
    );

    const invalid = {
      ...demoReviewFixture,
      validation: { ...demoReviewFixture.validation, passed: false, errors: 1 },
      findings: [
        ...demoReviewFixture.findings,
        {
          rule_id: "review-rule-coverage",
          severity: "error",
          message: "Update projection is not covered.",
          subject_id: null,
          subject_name: null,
          subject_kind: null,
          fixable: false,
        },
      ],
    };
    expect(canApproveReview(invalid, "family:independent-reasoner").reason).toMatch(
      /semantic finding/i,
    );
  });
});

describe("review presentation helpers", () => {
  it("groups semantic changes without mutating source order", () => {
    const original = demoReviewFixture.changes.items.map((change) => change.id);
    const grouped = groupChanges(demoReviewFixture.changes.items);

    expect(grouped.added).toHaveLength(5);
    expect(grouped.modified).toHaveLength(1);
    expect(grouped.removed).toHaveLength(1);
    expect(demoReviewFixture.changes.items.map((change) => change.id)).toEqual(original);
  });

  it("searches semantic labels and tier metadata", () => {
    const changedWeight = demoReviewFixture.changes.items.find(
      (change) => change.change === "modified",
    );
    expect(changedWeight).toBeDefined();
    expect(matchesReviewQuery(changedWeight!, "confidence")).toBe(true);
    expect(matchesReviewQuery(changedWeight!, "tier_2")).toBe(true);
    expect(matchesReviewQuery(changedWeight!, "unrelated")).toBe(false);
  });

  it("shortens both Git SHAs and labeled KG hashes for display only", () => {
    expect(shortHash(demoReviewFixture.repository.head_sha)).toBe("7ea9c6b2");
    expect(shortHash(demoReviewFixture.snapshot_identity.head_hash!, 10)).toBe("3f1e93775c");
  });
});
