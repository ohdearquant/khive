import { describe, expect, it } from "vitest";

import { demoReviewFixture } from "@/lib/fixtures/demo-review";
import { reviewActivityEvents, reviewActivityFacet, REVIEW_ACTIVITY_FACETS } from "@/lib/review-activity";
import type { ReviewReport } from "@/lib/review-bundle";

describe("one review-activity timeline", () => {
  it("interleaves dated types chronologically even when source pages arrive out of order", () => {
    const bundle = {
      ...demoReviewFixture,
      evidence: {
        ...demoReviewFixture.evidence,
        items: [...demoReviewFixture.evidence.items].reverse(),
      },
      activity: {
        ...demoReviewFixture.activity,
        items: [...demoReviewFixture.activity.items].reverse(),
      },
      change_set: {
        ...demoReviewFixture.change_set,
        operations: [...demoReviewFixture.change_set.operations].reverse(),
      },
    };
    const events = reviewActivityEvents(bundle);
    const dated = events.filter((event) => event.occurredAt !== null);

    expect(dated.map((event) => event.occurredAt)).toEqual(
      [...dated.map((event) => event.occurredAt)].sort((left, right) => Number(left) - Number(right)),
    );
    expect(dated.filter((event) => event.kind === "operations").map((event) => event.sequence))
      .toEqual([...demoReviewFixture.change_set.operations.map((operation) => operation.index)]);
    expect(events.some((event) => event.kind === "validation" && event.occurredAt === null)).toBe(true);
    expect(events.some((event) => event.kind === "review" && event.occurredAt === null)).toBe(true);
  });

  it("one_timeline_facets_filter_in_place", () => {
    const events = reviewActivityEvents(demoReviewFixture);
    expect(reviewActivityFacet(events, "all")).toEqual(events);
    expect(reviewActivityFacet(events, "all")).not.toBe(events);
    for (const facet of REVIEW_ACTIVITY_FACETS.filter((item) => item !== "all")) {
      expect(reviewActivityFacet(events, facet).every((event) => event.kind === facet)).toBe(true);
    }
    expect(REVIEW_ACTIVITY_FACETS.filter((item) => item !== "all")
      .flatMap((facet) => reviewActivityFacet(events, facet))).toHaveLength(events.length);
    expect(reviewActivityFacet(events, "operations")).not.toHaveLength(events.length);
    expect(reviewActivityFacet(events, "all")).toHaveLength(events.length);
  });

  it("the shared changeset report uses the same operation and review timeline", () => {
    const report: ReviewReport = {
      schema_version: "khive.review.v1",
      review_kind: "changeset",
      capability: demoReviewFixture.capability,
      change_set: demoReviewFixture.change_set,
      tier_summary: demoReviewFixture.tier_summary,
      validation: demoReviewFixture.validation,
      findings: demoReviewFixture.findings,
      review_gate: demoReviewFixture.review_gate,
    };
    const events = reviewActivityEvents(report);
    expect(events.filter((event) => event.kind === "operations")).toHaveLength(report.change_set.operations.length);
    expect(events.some((event) => event.kind === "review" && event.title.includes("citation-date-lint"))).toBe(true);
    expect(events.some((event) => event.kind === "validation")).toBe(true);
    expect(events.every((event) => event.kind !== "evidence" && event.kind !== "conversation")).toBe(true);
  });
});
