/**
 * Build the checked-in Draft 2020-12 schema from the editor's Zod wire model.
 *
 * Zod emits the complete closed structural shape. The additions below express
 * cross-field constraints that JSON Schema can represent without non-standard
 * `$data` extensions. The remaining derived-count invariants are named in the
 * schema annotation and enforced by the Zod/Rust conformance tests.
 */
export function buildReviewJsonSchema(reviewInputSchema, z) {
  const schema = structuredClone(z.toJSONSchema(reviewInputSchema));

  schema.$id = "https://khive.ai/schemas/khive-review-v1.schema.json";
  schema.title = "khive.review.v1";
  schema.description =
    "Read-only semantic KG review contract. review_kind discriminates the minimal CLI report from pull-request enrichment.";
  schema["x-khive-cross-field-invariants"] = [
    "tier counts cover every ordered operation",
    "complete semantic-change page counts agree with summary",
    "validation.passed agrees with validation.errors",
    "a PR head differing from repository.head_sha is stale and cannot be approved",
  ];

  const pullRequest = schema.oneOf.find(
    (candidate) => candidate.properties?.review_kind?.const === "pull_request",
  );
  if (!pullRequest) {
    throw new Error("pull_request review schema was not generated");
  }

  const snapshot = pullRequest.properties.snapshot_identity;
  snapshot.allOf = [
    {
      if: {
        properties: { hash_status: { const: "unavailable" } },
        required: ["hash_status"],
      },
      then: {
        properties: {
          algorithm: { type: "null" },
          base_hash: { type: "null" },
          head_hash: { type: "null" },
        },
      },
    },
    {
      if: {
        properties: { hash_status: { const: "fixture" } },
        required: ["hash_status"],
      },
      then: {
        properties: {
          algorithm: { type: "string", minLength: 1 },
          base_hash: { type: "string", pattern: "^sha256:[0-9a-f]{64}$" },
          head_hash: { type: "string", pattern: "^sha256:[0-9a-f]{64}$" },
        },
      },
    },
  ];

  const unavailable = (statusField, targetProperties) => ({
    if: {
      properties: {
        enrichment_status: {
          properties: { [statusField]: { const: "unavailable" } },
          required: [statusField],
        },
      },
      required: ["enrichment_status"],
    },
    then: { properties: targetProperties },
  });
  const emptyPage = { properties: { items: { maxItems: 0 } } };

  pullRequest.allOf = [
    unavailable("semantic_changes", { changes: emptyPage }),
    unavailable("evidence", { evidence: emptyPage }),
    unavailable("affected_graph", {
      graph: {
        properties: {
          nodes: emptyPage,
          edges: emptyPage,
        },
      },
    }),
    unavailable("commits", { commits: emptyPage }),
    unavailable("activity", { activity: emptyPage }),
    unavailable("retrieval", {
      retrieval: {
        properties: {
          search: emptyPage,
          recall: emptyPage,
          traversal: emptyPage,
        },
      },
    }),
  ];

  return schema;
}
