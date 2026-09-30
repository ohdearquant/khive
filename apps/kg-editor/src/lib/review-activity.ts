import type { ReviewInput } from "@/lib/review-bundle";

export const REVIEW_ACTIVITY_FACETS = [
  "all",
  "operations",
  "validation",
  "evidence",
  "review",
  "conversation",
] as const;

export type ReviewActivityFacet = (typeof REVIEW_ACTIVITY_FACETS)[number];
export type ReviewActivityKind = Exclude<ReviewActivityFacet, "all">;

export type ReviewActivityEvent = Readonly<{
  id: string;
  kind: ReviewActivityKind;
  title: string;
  detail: string;
  occurredAt: number | null;
  subjectId: string | null;
  sequence: number;
}>;

function stagedAtMilliseconds(input: ReviewInput): number {
  return Math.trunc(input.change_set.envelope.staged_at / 1_000);
}

/**
 * Keep absent event times absent. A captured bundle contains checks and
 * findings but gives neither an execution timestamp; placing them after the
 * dated stream tells the reader their relative time is unknown.
 */
export function reviewActivityEvents(input: ReviewInput): ReviewActivityEvent[] {
  const events: ReviewActivityEvent[] = [];
  const stagedAt = stagedAtMilliseconds(input);

  for (const operation of input.change_set.operations) {
    events.push({
      id: `operation:${operation.index}:${operation.id}`,
      kind: "operations",
      title: operation.summary,
      detail: operation.reason,
      occurredAt: stagedAt,
      subjectId: operation.id,
      sequence: operation.index,
    });
  }

  if (input.review_kind === "pull_request") {
    events.push({
      id: `review:pull-request:${input.pull_request.number}`,
      kind: "review",
      title: `Review #${input.pull_request.number} opened`,
      detail: input.pull_request.title,
      occurredAt: Date.parse(input.pull_request.created_at),
      subjectId: null,
      sequence: 0,
    });
    for (const [index, evidence] of input.evidence.items.entries()) {
      events.push({
        id: `evidence:${evidence.id}`,
        kind: "evidence",
        title: evidence.title,
        detail: `${evidence.source} · ${evidence.excerpt}`,
        occurredAt: Date.parse(evidence.captured_at),
        subjectId: evidence.id,
        sequence: index,
      });
    }
    for (const [index, activity] of input.activity.items.entries()) {
      events.push({
        id: `conversation:${activity.id}`,
        kind: "conversation",
        title: `${activity.actor} · ${activity.action}`,
        detail: activity.body,
        occurredAt: Date.parse(activity.created_at),
        subjectId: null,
        sequence: index,
      });
    }
    for (const [index, check] of input.checks.items.entries()) {
      events.push({
        id: `validation:${check.id}`,
        kind: "validation",
        title: `${check.label} · ${check.status}`,
        detail: check.detail,
        occurredAt: null,
        subjectId: null,
        sequence: index,
      });
    }
  } else {
    events.push({
      id: "validation:summary",
      kind: "validation",
      title: input.validation.passed ? "Validation passed" : "Validation failed",
      detail: `${input.validation.errors} errors · ${input.validation.warnings} warnings`,
      occurredAt: null,
      subjectId: null,
      sequence: 0,
    });
  }

  for (const [index, finding] of input.findings.entries()) {
    events.push({
      id: `review:finding:${finding.rule_id}:${index}`,
      kind: "review",
      title: `${finding.rule_id} · ${finding.severity}`,
      detail: finding.message,
      occurredAt: null,
      subjectId: finding.subject_id,
      sequence: index + 1,
    });
  }
  events.push({
    id: "review:gate",
    kind: "review",
    title: input.review_gate.status.replaceAll("_", " "),
    detail: input.review_gate.reason,
    occurredAt: null,
    subjectId: null,
    sequence: input.findings.length + 1,
  });

  const kindOrder: Record<ReviewActivityKind, number> = {
    operations: 0,
    validation: 1,
    evidence: 2,
    review: 3,
    conversation: 4,
  };
  return events.sort((left, right) => {
    if (left.occurredAt === null) return right.occurredAt === null
      ? kindOrder[left.kind] - kindOrder[right.kind] || left.sequence - right.sequence
      : 1;
    if (right.occurredAt === null) return -1;
    return left.occurredAt - right.occurredAt ||
      kindOrder[left.kind] - kindOrder[right.kind] ||
      left.sequence - right.sequence;
  });
}

export function reviewActivityFacet(
  events: readonly ReviewActivityEvent[],
  facet: ReviewActivityFacet,
): ReviewActivityEvent[] {
  return facet === "all" ? [...events] : events.filter((event) => event.kind === facet);
}
