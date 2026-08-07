import type { ReviewBundle, ReviewChange } from "@/lib/review-bundle";

export type ReviewDecision = "pending" | "approved" | "changes_requested";

const LOCALLY_RESOLVABLE_REVIEWER_GATE_STATUSES = new Set([
  "awaiting_independent_reviewer",
  "reviewer_required",
  "ineligible_same_model_family",
]);

export function canApproveReview(
  bundle: ReviewBundle,
  reviewerModelFamily: string,
): { allowed: boolean; reason: string } {
  if (bundle.pull_request.head_sha !== bundle.repository.head_sha) {
    return {
      allowed: false,
      reason: "The pull-request head changed; regenerate the semantic review bundle.",
    };
  }

  const producerFamily = bundle.change_set.envelope.producer_model_family;
  const reviewerFamily = reviewerModelFamily.trim();
  if (reviewerFamily === producerFamily) {
    return {
      allowed: false,
      reason: `ADR-102 requires a reviewer outside ${producerFamily}.`,
    };
  }

  const errorFindings = bundle.findings.filter(
    (finding) => finding.severity.toLocaleLowerCase() === "error",
  );
  if (!bundle.validation.passed || bundle.validation.errors > 0 || errorFindings.length > 0) {
    const count = Math.max(bundle.validation.errors, errorFindings.length, 1);
    return {
      allowed: false,
      reason: `${count} error-level semantic finding${count === 1 ? "" : "s"} block approval.`,
    };
  }

  const independentReviewerSelected = reviewerFamily.length > 0 && reviewerFamily !== producerFamily;
  const reviewerGateResolvedLocally =
    bundle.review_gate.required &&
    independentReviewerSelected &&
    LOCALLY_RESOLVABLE_REVIEWER_GATE_STATUSES.has(bundle.review_gate.status);
  if (
    (!bundle.review_gate.approval_ready || !bundle.review_gate.eligible) &&
    !reviewerGateResolvedLocally
  ) {
    return {
      allowed: false,
      reason: bundle.review_gate.reason || "The semantic review gate blocks approval.",
    };
  }

  const failedChecks = bundle.checks.items.filter((check) => check.status === "fail");
  if (failedChecks.length > 0) {
    return {
      allowed: false,
      reason: `${failedChecks.length} required check${failedChecks.length === 1 ? "" : "s"} failed.`,
    };
  }

  const pendingChecks = bundle.checks.items.filter(
    (check) =>
      check.status === "pending" &&
      !(check.id === "review-family" && independentReviewerSelected),
  );
  if (pendingChecks.length > 0) {
    return {
      allowed: false,
      reason: `${pendingChecks.length} required check${pendingChecks.length === 1 ? " is" : "s are"} still pending.`,
    };
  }

  return {
    allowed: true,
    reason: "Independent reviewer and required checks are satisfied.",
  };
}

export function groupChanges(changes: ReviewChange[]) {
  return {
    added: changes.filter((change) => change.change === "added"),
    modified: changes.filter((change) => change.change === "modified"),
    removed: changes.filter((change) => change.change === "removed"),
  };
}

export function matchesReviewQuery(change: ReviewChange, query: string): boolean {
  const normalized = query.trim().toLocaleLowerCase();
  if (!normalized) return true;
  return [change.title, change.subtitle, change.substrate, change.change, change.tier]
    .join(" ")
    .toLocaleLowerCase()
    .includes(normalized);
}

export function shortHash(hash: string, length = 8): string {
  const value = hash.includes(":") ? hash.split(":").at(-1) ?? hash : hash;
  return value.slice(0, length);
}
