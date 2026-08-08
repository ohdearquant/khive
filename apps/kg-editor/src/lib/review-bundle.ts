import { z } from "zod";

const tierSchema = z.enum(["tier_1", "tier_2"]);
const statusSchema = z.enum(["pass", "warning", "fail", "pending"]);
const substrateSchema = z.enum(["entity", "edge", "note"]);
const recordSchema = z.record(z.string(), z.unknown());

/** Browser-facing limits for the local review slice. */
export const REVIEW_PAGE_MAX_ITEMS = 200;
export const REVIEW_CORE_MAX_ITEMS = 500;
export const REVIEW_IMPORT_MAX_BYTES = 2 * 1024 * 1024;

function pageSchema<T extends z.ZodType>(item: T) {
  return z.strictObject({
    items: z.array(item).max(REVIEW_PAGE_MAX_ITEMS),
    next_cursor: z.string().nullable(),
    truncated: z.boolean(),
  });
}

const operationSchema = z.strictObject({
  index: z.number().int().nonnegative(),
  id: z.string().min(1),
  op: z.enum(["create", "link", "update", "delete", "merge"]),
  target: substrateSchema,
  tier: tierSchema,
  summary: z.string().min(1),
  reason: z.string().min(1),
  tier_reasons: z.array(z.string()).max(32),
  entity_ids: z.array(z.string()).max(100),
  before: recordSchema.optional(),
  after: recordSchema.optional(),
});

const envelopeSchema = z.strictObject({
  schema_version: z.number().int().positive(),
  producer: z.string().min(1),
  producer_model_family: z.string().min(1),
  staged_at: z.number().int().safe().nonnegative(),
  batch_id: z.string().min(1).optional(),
});

const coreCapabilitySchema = z.strictObject({
  source: z.string().min(1),
  mutability: z.string().min(1),
  no_writes: z.literal(true),
  git_reads: z.boolean(),
  khive_reads: z.boolean(),
  github_writes: z.boolean(),
  wasm: z.boolean(),
  persistence: z.boolean(),
  unavailable_actions: z.array(z.string()).length(5),
});

const tierSummarySchema = z.strictObject({
  operations: z.number().int().nonnegative(),
  tier_1: z.number().int().nonnegative(),
  tier_2: z.number().int().nonnegative(),
  highest_tier: tierSchema,
  requires_independent_review: z.boolean(),
  policy: z.string(),
});

const validationSchema = z.strictObject({
  scope: z.string(),
  rules_evaluated: z.number().int().nonnegative(),
  failed_rules: z.number().int().nonnegative(),
  errors: z.number().int().nonnegative(),
  warnings: z.number().int().nonnegative(),
  info: z.number().int().nonnegative(),
  passed: z.boolean(),
});

const findingSchema = z.strictObject({
  rule_id: z.string(),
  severity: z.string(),
  message: z.string(),
  subject_id: z.string().nullable(),
  subject_name: z.string().nullable(),
  subject_kind: z.string().nullable(),
  fixable: z.boolean(),
});

const reviewGateSchema = z.strictObject({
  required: z.boolean(),
  producer_model_family: z.string(),
  reviewer_model_family: z.string().nullable(),
  eligible: z.boolean(),
  approval_ready: z.boolean(),
  status: z.string(),
  reason: z.string(),
  persisted: z.literal(false),
});

const changeSchema = z.strictObject({
  id: z.string(),
  substrate: substrateSchema,
  change: z.enum(["added", "modified", "removed"]),
  title: z.string(),
  subtitle: z.string(),
  tier: tierSchema,
  fields: z.array(
    z.strictObject({
      path: z.string(),
      before: z.unknown().optional(),
      after: z.unknown().optional(),
    }),
  ),
  evidence_ids: z.array(z.string()),
});

const graphNodeSchema = z.strictObject({
  id: z.string(),
  label: z.string(),
  kind: z.string(),
  state: z.enum(["added", "modified", "removed", "context"]),
  x: z.number(),
  y: z.number(),
  description: z.string(),
});

/** Minimal shared report emitted by `khive kg review --format json`. */
export const reviewReportSchema = z.strictObject({
  schema_version: z.literal("khive.review.v1"),
  review_kind: z.literal("changeset"),
  capability: coreCapabilitySchema,
  change_set: z.strictObject({
    envelope: envelopeSchema,
    operations: z.array(operationSchema).max(REVIEW_CORE_MAX_ITEMS),
  }),
  tier_summary: tierSummarySchema,
  validation: validationSchema,
  findings: z.array(findingSchema).max(REVIEW_CORE_MAX_ITEMS),
  review_gate: reviewGateSchema,
}).superRefine((value, context) => {
  if (value.tier_summary.operations !== value.change_set.operations.length ||
      value.tier_summary.tier_1 + value.tier_summary.tier_2 !== value.change_set.operations.length) {
    context.addIssue({ code: "custom", path: ["tier_summary"], message: "tier counts must cover every ordered operation" });
  }
  if (value.validation.passed !== (value.validation.errors === 0)) {
    context.addIssue({ code: "custom", path: ["validation", "passed"], message: "passed must agree with the error count" });
  }
});

/** Pull-request enrichment of the exact same shared report core. */
export const reviewBundleSchema = z.strictObject({
  schema_version: z.literal("khive.review.v1"),
  review_kind: z.literal("pull_request"),
  generated_at: z.iso.datetime(),
  capability: coreCapabilitySchema.extend({
    source: z.enum(["fixture", "import"]),
    label: z.string(),
  }),
  repository: z.strictObject({
    owner: z.string(),
    name: z.string(),
    visibility: z.enum(["public", "private"]),
    default_branch: z.string(),
    head_branch: z.string(),
    base_sha: z.string().regex(/^[0-9a-f]{40}$/),
    head_sha: z.string().regex(/^[0-9a-f]{40}$/),
  }),
  snapshot_identity: z.strictObject({
    coverage: z.strictObject({
      entities: z.literal(true),
      edges: z.literal(true),
      notes: z.literal(false),
    }),
    hash_status: z.enum(["fixture", "unavailable"]),
    algorithm: z.string().nullable(),
    base_hash: z.string().regex(/^sha256:[0-9a-f]{64}$/).nullable(),
    head_hash: z.string().regex(/^sha256:[0-9a-f]{64}$/).nullable(),
  }),
  pull_request: z.strictObject({
    number: z.number().int().positive(),
    title: z.string(),
    body: z.string(),
    state: z.enum(["open", "merged", "closed", "draft"]),
    author: z.string(),
    created_at: z.iso.datetime(),
    head_sha: z.string().regex(/^[0-9a-f]{40}$/),
  }),
  live_proposal: z.strictObject({ proposal_id: z.uuid() }).nullable(),
  enrichment_status: z.strictObject({
    semantic_changes: z.enum(["available", "unavailable"]),
    evidence: z.enum(["available", "unavailable"]),
    affected_graph: z.enum(["available", "unavailable"]),
    commits: z.enum(["available", "unavailable"]),
    activity: z.enum(["available", "unavailable"]),
    retrieval: z.enum(["captured", "live", "unavailable"]),
  }),
  change_set: z.strictObject({
    envelope: envelopeSchema,
    operations: z.array(operationSchema).max(REVIEW_CORE_MAX_ITEMS),
  }),
  tier_summary: tierSummarySchema,
  validation: validationSchema,
  findings: z.array(findingSchema).max(REVIEW_CORE_MAX_ITEMS),
  review_gate: reviewGateSchema,
  summary: z.strictObject({
    entities_added: z.number().int().nonnegative(),
    entities_modified: z.number().int().nonnegative(),
    entities_removed: z.number().int().nonnegative(),
    edges_added: z.number().int().nonnegative(),
    edges_modified: z.number().int().nonnegative(),
    edges_removed: z.number().int().nonnegative(),
    tier_1: z.number().int().nonnegative(),
    tier_2: z.number().int().nonnegative(),
  }),
  checks: pageSchema(
    z.strictObject({
      id: z.string(),
      label: z.string(),
      status: statusSchema,
      detail: z.string(),
      duration_ms: z.number().nonnegative(),
    }),
  ),
  changes: pageSchema(changeSchema),
  evidence: pageSchema(
    z.strictObject({
      id: z.string(),
      title: z.string(),
      source: z.string(),
      locator: z.string(),
      excerpt: z.string(),
      captured_at: z.iso.datetime(),
    }),
  ),
  graph: z.strictObject({
    nodes: pageSchema(graphNodeSchema),
    edges: pageSchema(
      z.strictObject({
        id: z.string(),
        source: z.string(),
        target: z.string(),
        relation: z.string(),
        state: z.enum(["added", "modified", "removed", "context"]),
        weight: z.number().min(0).max(1),
      }),
    ),
  }),
  commits: pageSchema(
    z.strictObject({
      sha: z.string(),
      subject: z.string(),
      author: z.string(),
      created_at: z.iso.datetime(),
      state: z.enum(["head", "base", "ancestor"]),
    }),
  ),
  activity: pageSchema(
    z.strictObject({
      id: z.string(),
      actor: z.string(),
      action: z.string(),
      body: z.string(),
      created_at: z.iso.datetime(),
      tone: z.enum(["neutral", "positive", "warning"]),
    }),
  ),
  retrieval: z.strictObject({
    search: pageSchema(
      z.strictObject({
        id: z.string(),
        title: z.string(),
        kind: z.string(),
        score: z.number(),
        snippet: z.string(),
      }),
    ),
    recall: pageSchema(
      z.strictObject({
        id: z.string(),
        score: z.number(),
        content: z.string(),
        memory_type: z.string(),
      }),
    ),
    traversal: pageSchema(
      z.strictObject({
        depth: z.number().int().nonnegative(),
        id: z.string(),
        name: z.string(),
        kind: z.string(),
        via: z.string().nullable(),
      }),
    ),
  }),
}).superRefine((value, context) => {
  const snapshot = value.snapshot_identity;
  const hasCompleteHashes = Boolean(snapshot.algorithm && snapshot.base_hash && snapshot.head_hash);
  if (snapshot.hash_status === "unavailable" && (snapshot.algorithm || snapshot.base_hash || snapshot.head_hash)) {
    context.addIssue({ code: "custom", path: ["snapshot_identity"], message: "unavailable snapshot identity cannot carry hashes or an algorithm" });
  }
  if (snapshot.hash_status !== "unavailable" && !hasCompleteHashes) {
    context.addIssue({ code: "custom", path: ["snapshot_identity"], message: "fixture snapshot identities require an algorithm and both hashes" });
  }

  const operations = value.change_set.operations.length;
  if (value.tier_summary.operations !== operations ||
      value.tier_summary.tier_1 + value.tier_summary.tier_2 !== operations ||
      value.summary.tier_1 !== value.tier_summary.tier_1 ||
      value.summary.tier_2 !== value.tier_summary.tier_2) {
    context.addIssue({ code: "custom", path: ["tier_summary"], message: "tier counts must agree across operations and presentation summary" });
  }
  if (value.validation.passed !== (value.validation.errors === 0)) {
    context.addIssue({ code: "custom", path: ["validation", "passed"], message: "passed must agree with the error count" });
  }

  const semanticCounts = {
    entities_added: value.changes.items.filter((change) => change.substrate === "entity" && change.change === "added").length,
    entities_modified: value.changes.items.filter((change) => change.substrate === "entity" && change.change === "modified").length,
    entities_removed: value.changes.items.filter((change) => change.substrate === "entity" && change.change === "removed").length,
    edges_added: value.changes.items.filter((change) => change.substrate === "edge" && change.change === "added").length,
    edges_modified: value.changes.items.filter((change) => change.substrate === "edge" && change.change === "modified").length,
    edges_removed: value.changes.items.filter((change) => change.substrate === "edge" && change.change === "removed").length,
  };
  if (!value.changes.next_cursor && !value.changes.truncated) {
    for (const [key, count] of Object.entries(semanticCounts)) {
      if (value.summary[key as keyof typeof semanticCounts] !== count) {
        context.addIssue({ code: "custom", path: ["summary", key], message: "summary count must match the complete semantic-change page" });
      }
    }
  }

  const unavailablePages = [
    [value.enrichment_status.semantic_changes, value.changes.items.length, "changes"],
    [value.enrichment_status.evidence, value.evidence.items.length, "evidence"],
    [value.enrichment_status.affected_graph, value.graph.nodes.items.length + value.graph.edges.items.length, "graph"],
    [value.enrichment_status.commits, value.commits.items.length, "commits"],
    [value.enrichment_status.activity, value.activity.items.length, "activity"],
    [value.enrichment_status.retrieval, value.retrieval.search.items.length + value.retrieval.recall.items.length + value.retrieval.traversal.items.length, "retrieval"],
  ] as const;
  for (const [status, itemCount, path] of unavailablePages) {
    if (status === "unavailable" && itemCount > 0) {
      context.addIssue({ code: "custom", path: [path], message: "unavailable enrichment must have an empty page" });
    }
  }
});

export const reviewInputSchema = z.discriminatedUnion("review_kind", [
  reviewReportSchema,
  reviewBundleSchema,
]);

export type ReviewBundle = z.infer<typeof reviewBundleSchema>;
export type ReviewReport = z.infer<typeof reviewReportSchema>;
export type ReviewInput = z.infer<typeof reviewInputSchema>;
export type ReviewChange = ReviewBundle["changes"]["items"][number];
export type GraphNode = ReviewBundle["graph"]["nodes"]["items"][number];

export function parseReviewBundle(value: unknown): ReviewBundle {
  return reviewBundleSchema.parse(value);
}

export function parseReviewInput(value: unknown): ReviewInput {
  return reviewInputSchema.parse(value);
}

export function isReviewReport(value: ReviewInput): value is ReviewReport {
  return value.review_kind === "changeset";
}
