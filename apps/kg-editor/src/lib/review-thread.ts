import type { ReviewBundle, ReviewInput } from "@/lib/review-bundle";

type ReviewOperation = ReviewInput["change_set"]["operations"][number];
type ReviewFinding = ReviewInput["findings"][number];
type ReviewChange = ReviewBundle["changes"]["items"][number];
type ReviewEvidence = ReviewBundle["evidence"]["items"][number];
type ReviewActivity = ReviewBundle["activity"]["items"][number];

/** Local annotations must name a unit; imported activity has no such link. */
export type ReviewAnnotation = {
  id: string;
  unitKey: string;
  actor: string;
  body: string;
  createdAt: string;
};

export type ReviewThreadEvent =
  | {
      kind: "operation";
      key: string;
      occurredAt: string;
      occurredAtUs: number;
      timeSource: "batch_staged_at";
      operation: ReviewOperation;
    }
  | {
      kind: "evidence";
      key: string;
      occurredAt: string;
      occurredAtUs: number;
      timeSource: "evidence_captured_at";
      evidence: ReviewEvidence;
    }
  | {
      kind: "annotation";
      key: string;
      occurredAt: string | null;
      occurredAtUs: number | null;
      timeSource: "annotation_created_at";
      annotation: ReviewAnnotation;
    }
  | {
      kind: "finding";
      key: string;
      occurredAt: null;
      occurredAtUs: null;
      timeSource: "unavailable";
      finding: ReviewFinding;
    };

export type ReviewEvidenceState = {
  status: "available" | "unlinked" | "unavailable" | "truncated" | "missing";
  linkedIds: string[];
  unresolvedIds: string[];
  reason: string;
};

export type ReviewThreadUnit = {
  /** Exact substrate and record ID; safe even if two substrates reuse an ID. */
  key: string;
  id: string;
  substrate: ReviewOperation["target"];
  title: string;
  subtitle: string;
  tier: ReviewOperation["tier"];
  operations: ReviewOperation[];
  change: ReviewChange | null;
  evidence: ReviewEvidenceState;
  /** Dated events in source-time order, followed by explicitly undated findings. */
  events: ReviewThreadEvent[];
};

export type ReviewThreadModel = {
  units: ReviewThreadUnit[];
  unlinkedFindings: ReviewFinding[];
  unlinkedAnnotations: ReviewAnnotation[];
  /** Evidence with no link in the loaded change page; a later page may still name it. */
  unassignedEvidence: ReviewEvidence[];
  unassignedEvidenceReason: string;
  /** PR activity has its own ID but no subject ID, so it cannot enter a unit thread. */
  unlinkedActivity: ReviewActivity[];
  activityStatus: "unavailable" | "empty" | "unlinked";
  activityReason: string;
};

export function reviewUnitKey(substrate: ReviewOperation["target"], id: string): string {
  return `${substrate}:${id}`;
}

function operationTitle(operation: ReviewOperation): string {
  const name = operation.after?.name ?? operation.before?.name;
  return typeof name === "string" && name.trim() ? name : operation.summary;
}

function emptyEvidence(reason: string, status: ReviewEvidenceState["status"] = "unavailable"): ReviewEvidenceState {
  return { status, linkedIds: [], unresolvedIds: [], reason };
}

function sourceTimeUs(iso: string): number | null {
  const milliseconds = Date.parse(iso);
  return Number.isFinite(milliseconds) ? milliseconds * 1_000 : null;
}

function compareThreadEvents(left: ReviewThreadEvent, right: ReviewThreadEvent): number {
  if (left.occurredAtUs === null) return right.occurredAtUs === null ? 0 : 1;
  if (right.occurredAtUs === null) return -1;
  if (left.occurredAtUs !== right.occurredAtUs) return left.occurredAtUs - right.occurredAtUs;
  if (left.kind === "operation" && right.kind === "operation") {
    return left.operation.index - right.operation.index;
  }
  const rank = { operation: 0, evidence: 1, annotation: 2, finding: 3 };
  return rank[left.kind] - rank[right.kind];
}

function evidenceForUnit(
  bundle: ReviewBundle,
  unit: ReviewThreadUnit,
  evidenceById: Map<string, ReviewEvidence>,
): ReviewEvidenceState {
  if (bundle.enrichment_status.semantic_changes === "unavailable") {
    return emptyEvidence("Semantic changes are unavailable; evidence links cannot be determined.");
  }
  if (!unit.change) {
    if (bundle.changes.truncated || bundle.changes.next_cursor) {
      return emptyEvidence("The semantic-change page is incomplete; this unit's evidence links may be on another page.", "truncated");
    }
    return emptyEvidence("No semantic change entry links evidence to this unit.");
  }

  const linkedIds = [...new Set(unit.change.evidence_ids)];
  if (linkedIds.length === 0) {
    return emptyEvidence("No evidence IDs are linked to this reviewable unit.", "unlinked");
  }
  if (bundle.enrichment_status.evidence === "unavailable") {
    return {
      status: "unavailable",
      linkedIds,
      unresolvedIds: linkedIds,
      reason: "Evidence enrichment is unavailable for these linked IDs.",
    };
  }
  const unresolvedIds = linkedIds.filter((id) => !evidenceById.has(id));
  if (unresolvedIds.length > 0) {
    const incomplete = bundle.evidence.truncated || Boolean(bundle.evidence.next_cursor);
    return {
      status: incomplete ? "truncated" : "missing",
      linkedIds,
      unresolvedIds,
      reason: incomplete
        ? "The evidence page is incomplete; linked evidence may be on another page."
        : "Linked evidence IDs are absent from the complete evidence page.",
    };
  }
  return { status: "available", linkedIds, unresolvedIds: [], reason: "Linked evidence is available." };
}

/** Build one list row and one evidence-aware thread per exact reviewable record. */
export function buildReviewThreadModel(
  input: ReviewInput,
  annotations: readonly ReviewAnnotation[] = [],
): ReviewThreadModel {
  const unitsByKey = new Map<string, ReviewThreadUnit>();
  const stagedAtUs = input.change_set.envelope.staged_at;
  const stagedAt = new Date(Math.floor(stagedAtUs / 1_000)).toISOString();

  // The operation index is the only order supplied for events sharing staged_at.
  const operations = [...input.change_set.operations].sort((left, right) => left.index - right.index);
  for (const operation of operations) {
    const key = reviewUnitKey(operation.target, operation.id);
    let unit = unitsByKey.get(key);
    if (!unit) {
      unit = {
        key,
        id: operation.id,
        substrate: operation.target,
        title: operationTitle(operation),
        subtitle: `${operation.target} · ${operation.op}`,
        tier: operation.tier,
        operations: [],
        change: null,
        evidence: emptyEvidence("This changeset report does not carry evidence enrichment."),
        events: [],
      };
      unitsByKey.set(key, unit);
    }
    unit.operations.push(operation);
    if (operation.tier === "tier_2") unit.tier = "tier_2";
    unit.events.push({
      kind: "operation",
      key: `operation:${operation.index}:${operation.id}`,
      occurredAt: stagedAt,
      occurredAtUs: stagedAtUs,
      timeSource: "batch_staged_at",
      operation,
    });
  }

  if (input.review_kind === "pull_request") {
    for (const change of input.changes.items) {
      const key = reviewUnitKey(change.substrate, change.id);
      let unit = unitsByKey.get(key);
      if (!unit) {
        unit = {
          key,
          id: change.id,
          substrate: change.substrate,
          title: change.title,
          subtitle: change.subtitle,
          tier: change.tier,
          operations: [],
          change: null,
          evidence: emptyEvidence("Evidence links have not been evaluated."),
          events: [],
        };
        unitsByKey.set(key, unit);
      }
      unit.change = change;
      unit.title = change.title || unit.title;
      unit.subtitle = change.subtitle || unit.subtitle;
      if (change.tier === "tier_2") unit.tier = "tier_2";
    }

    const evidenceById = new Map(input.evidence.items.map((item) => [item.id, item]));
    for (const unit of unitsByKey.values()) {
      unit.evidence = evidenceForUnit(input, unit, evidenceById);
      for (const id of unit.evidence.linkedIds) {
        const evidence = evidenceById.get(id);
        if (!evidence) continue;
        unit.events.push({
          kind: "evidence",
          key: `evidence:${id}`,
          occurredAt: evidence.captured_at,
          occurredAtUs: sourceTimeUs(evidence.captured_at)!,
          timeSource: "evidence_captured_at",
          evidence,
        });
      }
    }
  }

  // A finding's subject_id names a record, but its schema has no substrate or timestamp.
  // Only an unambiguous exact ID can attach it; it remains explicitly undated.
  const unitsById = new Map<string, ReviewThreadUnit[]>();
  for (const unit of unitsByKey.values()) {
    unitsById.set(unit.id, [...(unitsById.get(unit.id) ?? []), unit]);
  }
  const unlinkedFindings: ReviewFinding[] = [];
  for (const [index, finding] of input.findings.entries()) {
    const matches = finding.subject_id ? unitsById.get(finding.subject_id) : undefined;
    if (!matches || matches.length !== 1) {
      unlinkedFindings.push(finding);
      continue;
    }
    matches[0].events.push({
      kind: "finding",
      key: `finding:${index}:${finding.rule_id}`,
      occurredAt: null,
      occurredAtUs: null,
      timeSource: "unavailable",
      finding,
    });
  }

  const unlinkedAnnotations: ReviewAnnotation[] = [];
  for (const annotation of annotations) {
    const unit = unitsByKey.get(annotation.unitKey);
    if (!unit) {
      unlinkedAnnotations.push(annotation);
      continue;
    }
    const occurredAtUs = sourceTimeUs(annotation.createdAt);
    unit.events.push({
      kind: "annotation",
      key: `annotation:${annotation.id}`,
      occurredAt: occurredAtUs === null ? null : annotation.createdAt,
      occurredAtUs,
      timeSource: "annotation_created_at",
      annotation,
    });
  }

  const units = [...unitsByKey.values()];
  for (const unit of units) unit.events.sort(compareThreadEvents);

  const visibleEvidenceIds = input.review_kind === "pull_request"
    ? new Set(input.changes.items.flatMap((change) => change.evidence_ids))
    : new Set<string>();
  const unassignedEvidence = input.review_kind === "pull_request"
    ? input.evidence.items.filter((item) => !visibleEvidenceIds.has(item.id))
    : [];
  const unassignedEvidenceReason = input.review_kind === "changeset"
    ? "This changeset report has no evidence enrichment."
    : input.enrichment_status.semantic_changes === "unavailable"
      ? "Semantic changes are unavailable, so evidence cannot be assigned to a unit."
      : input.changes.truncated || input.changes.next_cursor
        ? "These evidence items have no link in the loaded change page; another page may link them."
        : "No semantic change links these evidence items to a reviewable unit.";

  const unlinkedActivity = input.review_kind === "pull_request" ? input.activity.items : [];
  const activityStatus = input.review_kind === "changeset" ||
    input.enrichment_status.activity === "unavailable"
    ? "unavailable"
    : unlinkedActivity.length > 0 ? "unlinked" : "empty";
  const activityReason = activityStatus === "unavailable"
    ? "Imported review activity is unavailable."
    : activityStatus === "unlinked"
      ? "Imported review activity has no subject ID and cannot be assigned to a unit thread."
      : "No imported review activity is present.";

  return {
    units,
    unlinkedFindings,
    unlinkedAnnotations,
    unassignedEvidence,
    unassignedEvidenceReason,
    unlinkedActivity,
    activityStatus,
    activityReason,
  };
}
