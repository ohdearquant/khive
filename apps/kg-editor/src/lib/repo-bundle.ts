import { z } from "zod";

import {
  addressableModulePathIssue,
  publicRepositoryUrlIssue,
} from "@/lib/repository-location";

// Generic strings intentionally have no local minLength: the normative JSON
// Schema does not add one. Shape closure and field-specific formats stay strict.
const wireString = z.string();
const sha = z.string().regex(/^[0-9a-f]{40}$/);
const shortShaSchema = z.string().regex(/^[0-9a-f]{7,40}$/);
const timestamp = z.iso.datetime({ offset: true });
const boundedItems = 50_000;

const granularitySchema = z.enum(["repository", "module", "module_symbol_deferred"]);
const joinTagSchema = z.enum(["history_only", "structure_only", "join", "field_tagged"]);
const viewStatusSchema = z.enum(["available", "unavailable"]);
const sourceCoverageSchema = z.discriminatedUnion("state", [
  z.strictObject({ state: z.literal("completed") }),
  z.strictObject({ state: z.literal("stopped_early"), reason: wireString }),
  z.strictObject({ state: z.literal("skipped"), reason: wireString }),
  z.strictObject({ state: z.literal("unrequested") }),
  z.strictObject({ state: z.literal("unknown"), reason: wireString }),
]);

function availabilitySchema<T extends z.ZodType>(value: T) {
  return z.discriminatedUnion("status", [
    z.strictObject({ status: z.literal("available"), value }),
    z.strictObject({ status: z.literal("unavailable"), reason: wireString }),
  ]);
}

const pageBoundSchema = z.strictObject({
  kind: z.enum(["all", "top_n"]),
  max_items: z.number().int().nonnegative().max(boundedItems),
  order: wireString,
});

const disclosureSchema = z.strictObject({
  status: z.enum(["complete", "truncated", "unavailable"]),
  reason: z.string().nullable().optional(),
});

function pageSchema<T extends z.ZodType>(item: T) {
  return z.strictObject({
    items: z.array(item).max(boundedItems),
    total_count: availabilitySchema(z.number().int().nonnegative()),
    bound: pageBoundSchema,
    next_cursor: z.string().nullable().optional(),
    truncated: z.boolean(),
    disclosure: disclosureSchema,
  }).superRefine((page, context) => {
    if (page.items.length > page.bound.max_items) {
      context.addIssue({ code: "custom", path: ["items"], message: "page items exceed the declared bound" });
    }
    if (page.truncated !== (page.disclosure.status === "truncated")) {
      context.addIssue({ code: "custom", path: ["disclosure", "status"], message: "truncation flag and disclosure must agree" });
    }
    if (page.disclosure.status === "unavailable" &&
        (page.items.length !== 0 || page.total_count.status !== "unavailable")) {
      context.addIssue({ code: "custom", path: ["disclosure"], message: "unavailable pages cannot contain invented items or totals" });
    }
    if (page.total_count.status === "available" && !page.truncated && !page.next_cursor &&
        page.total_count.value !== page.items.length) {
      context.addIssue({ code: "custom", path: ["total_count"], message: "complete page total must equal its item count" });
    }
  });
}

const repositoryIdentitySchema = z.strictObject({
  host: wireString,
  owner: wireString,
  name: wireString,
  canonical_url: z.url(),
  default_branch: availabilitySchema(wireString),
});

const bundleMetaSchema = z.strictObject({
  repository: repositoryIdentitySchema,
  snapshot: z.strictObject({ head_sha: sha, ingested_at: timestamp }),
  producer: z.strictObject({
    exporter: wireString,
    kkernel_version: wireString,
    khive_pack_git_version: wireString,
    khive_pack_code_version: wireString,
  }),
  ingest: z.strictObject({
    git_digest: availabilitySchema(z.strictObject({
      calls: z.number().int().nonnegative(),
      history_exhausted: z.boolean(),
      cursor_stalled: z.boolean(),
      writes_refused: z.number().int().nonnegative(),
      changed_paths_filtered_noncanonical: z.number().int().nonnegative(),
      sources: z.strictObject({
        commits: sourceCoverageSchema,
        issues: sourceCoverageSchema,
        pull_requests: sourceCoverageSchema,
      }),
    })),
    code_ingest: availabilitySchema(z.strictObject({
      source_revision: sha,
      languages: z.array(wireString),
      blocked_count: z.number().int().nonnegative(),
      files_dropped_without_source_path: z.number().int().nonnegative(),
      files_skipped_without_module_path: z.number().int().nonnegative(),
      coverage_stamps_missed: z.number().int().nonnegative(),
      warnings_count: z.number().int().nonnegative(),
    })),
    clone_tags: sourceCoverageSchema,
  }),
});

const repositoryNodeSchema = z.strictObject({ id: wireString, label: wireString });
const packageNodeSchema = z.strictObject({
  id: wireString,
  name: wireString,
  languages: z.array(wireString),
});
const moduleNodeSchema = z.strictObject({
  id: wireString,
  package_id: wireString,
  name: wireString,
  language: wireString,
  module_path: wireString,
  source_path: wireString,
  source_revision: sha,
  content_hash: wireString,
  import_scan_status: wireString,
});
const symbolNodeSchema = z.strictObject({ id: wireString, module_id: wireString, name: wireString });
const commitNodeSchema = z.strictObject({
  id: wireString,
  sha,
  short_sha: shortShaSchema,
  author: wireString,
  committed_at: timestamp,
  parents: z.array(sha),
  subject: wireString,
});
const issueNodeSchema = z.strictObject({
  id: wireString,
  number: z.number().int().nonnegative(),
  title: wireString,
  author: availabilitySchema(wireString),
  created_at: availabilitySchema(timestamp),
  closed_at: availabilitySchema(timestamp),
  labels: z.array(z.string()),
});
const pullRequestNodeSchema = z.strictObject({
  id: wireString,
  number: z.number().int().nonnegative(),
  title: wireString,
  author: availabilitySchema(wireString),
  created_at: availabilitySchema(timestamp),
  merged_at: availabilitySchema(timestamp),
  closed_at: availabilitySchema(timestamp),
  base_ref: availabilitySchema(wireString),
  head_ref: availabilitySchema(wireString),
});
const graphEdgeSchema = z.strictObject({
  id: wireString,
  source: wireString,
  target: wireString,
  relation: wireString,
  weight: z.number().finite().min(0).max(1),
  origin: z.enum(["ingested", "derived"]),
  derivation: z.discriminatedUnion("method", [
    z.strictObject({ method: z.literal("changed_path_source_path_exact"), source_revision: sha, source_path: wireString }),
    z.strictObject({ method: z.literal("clone_path_fallback"), source_revision: sha, source_path: wireString }),
    z.strictObject({ method: z.literal("repository_package_normalization"), source_project: wireString }),
  ]).nullable(),
}).superRefine((edge, context) => {
  if ((edge.origin === "derived") !== Boolean(edge.derivation)) {
    context.addIssue({ code: "custom", path: ["derivation"], message: "only derived edges carry derivation evidence" });
  }
});

const joinResidualSchema = z.strictObject({
  side: z.enum(["path", "entity"]),
  source_project: wireString,
  module_path: z.string(),
  source_path: z.string(),
  reason: wireString,
});
const repositoryResolutionSchema = z.strictObject({
  repository: z.url(),
  language: wireString,
  files: z.number().int().nonnegative(),
  derived_keys: z.number().int().nonnegative(),
  entity_keys: z.number().int().nonnegative(),
  matched: z.number().int().nonnegative(),
  resolution_rate: availabilitySchema(z.number().min(0).max(1)),
  residuals: pageSchema(joinResidualSchema),
});

const moduleHistoryNavigationSchema = z.strictObject({
  module_id: wireString,
  commits: pageSchema(wireString),
  pull_requests: availabilitySchema(pageSchema(wireString)),
  issues: availabilitySchema(pageSchema(wireString)),
});
const commitHistoryNavigationSchema = z.strictObject({
  commit_id: wireString,
  modules: pageSchema(wireString),
});
const historicalPathResidualSchema = z.strictObject({
  commit_sha: sha,
  source_path: wireString,
  reason: wireString,
});

const repoGraphSchema = z.strictObject({
  repository: repositoryNodeSchema,
  packages: pageSchema(packageNodeSchema),
  modules: pageSchema(moduleNodeSchema),
  functions: pageSchema(symbolNodeSchema),
  datatypes: pageSchema(symbolNodeSchema),
  interfaces: pageSchema(symbolNodeSchema),
  commits: pageSchema(commitNodeSchema),
  issues: pageSchema(issueNodeSchema),
  pull_requests: pageSchema(pullRequestNodeSchema),
  structure_edges: pageSchema(graphEdgeSchema),
  history_edges: pageSchema(graphEdgeSchema),
  commit_module_edges: pageSchema(graphEdgeSchema),
  history_navigation: z.strictObject({
    by_module: pageSchema(moduleHistoryNavigationSchema),
    by_commit: pageSchema(commitHistoryNavigationSchema),
  }),
  join_resolution: z.strictObject({
    scope: z.strictObject({
      languages: z.array(wireString),
      python: availabilitySchema(z.boolean()),
      typescript: availabilitySchema(z.boolean()),
    }),
    repositories: availabilitySchema(z.array(repositoryResolutionSchema)),
    historical: availabilitySchema(z.array(z.strictObject({
      repository: z.url(),
      language: wireString,
      total_changed_paths: z.number().int().nonnegative(),
      rust_in_scope_paths: z.number().int().nonnegative(),
      matched_rust_paths: z.number().int().nonnegative(),
      out_of_scope_paths: z.number().int().nonnegative(),
      unresolved_rust_paths: pageSchema(historicalPathResidualSchema),
    }))),
  }),
});

const viewCapabilitySchema = z.strictObject({
  label: wireString,
  granularity: granularitySchema,
  join: joinTagSchema,
  status: viewStatusSchema,
  unavailable_reason: z.string().nullable().optional(),
});
const historyViewCapabilitySchema = viewCapabilitySchema.extend({
  commit_module_facet: availabilitySchema(z.boolean()),
  pull_request_module_facet: availabilitySchema(z.boolean()),
  issue_module_facet: availabilitySchema(z.boolean()),
});
const languageCapabilitySchema = z.strictObject({
  label: wireString,
  module_join: z.boolean(),
  measured: z.boolean(),
  reason: z.string().nullable().optional(),
});

const capabilitySchema = z.strictObject({
  mode: z.literal("static_showcase"),
  read_only: z.literal(true),
  writes: z.literal(false),
  live_queries: z.literal(false),
  on_demand_ingest: z.literal(false),
  languages: z.strictObject({
    rust: languageCapabilitySchema,
    python: languageCapabilitySchema,
    typescript: languageCapabilitySchema,
  }),
  labels: z.strictObject({
    product: wireString,
    input_placeholder: wireString,
    lookup_action: wireString,
    miss_title: wireString,
    miss_body: wireString,
    unavailable: wireString,
    truncated: wireString,
    derived: wireString,
    ingested: wireString,
    metrics: z.strictObject({
      change_frequency: wireString,
      fan_in: wireString,
      fan_out: wireString,
      cochange_count: wireString,
      support: wireString,
      source_files: wireString,
      recent_activity: wireString,
      week: wireString,
      commits: wireString,
      issues_opened: wireString,
      issues_closed: wireString,
      pull_requests_opened: wireString,
      pull_requests_merged: wireString,
      lead_time: wireString,
      p50: wireString,
      p90: wireString,
      p95: wireString,
      author_concentration: wireString,
      bus_factor: wireString,
      dependent_count: wireString,
      cycle_count: wireString,
      resolution: wireString,
      repository_age: wireString,
      package_count: wireString,
      module_count: wireString,
      symbol_count: wireString,
      activity_trend: wireString,
      top_hotspots: wireString,
      ownership_warnings: wireString,
    }),
    hotspot_quadrants: z.strictObject({
      high_churn_high_fan_in: wireString,
      high_churn_low_fan_in: wireString,
      low_churn_high_fan_in: wireString,
      low_churn_low_fan_in: wireString,
    }),
    node_types: z.strictObject({
      repository: wireString,
      package: wireString,
      module: wireString,
      function: wireString,
      datatype: wireString,
      interface: wireString,
      commit: wireString,
      issue: wireString,
      pull_request: wireString,
    }),
  }),
  views: z.strictObject({
    structure_graph: viewCapabilitySchema,
    history_structure_navigation: historyViewCapabilitySchema,
    dependency_topology: viewCapabilitySchema,
    hotspot_quadrant: viewCapabilitySchema,
    hidden_coupling: viewCapabilitySchema,
    structure_treemap: viewCapabilitySchema,
    cadence_timeline: viewCapabilitySchema,
    ownership: viewCapabilitySchema,
    api_surface: viewCapabilitySchema,
    scorecard: viewCapabilitySchema,
  }),
});

const analysisWindowSchema = z.strictObject({
  kind: z.enum(["all_history", "rolling_days", "range"]),
  start: timestamp.nullable().optional(),
  end: timestamp.nullable().optional(),
  days: z.number().int().nonnegative().nullable().optional(),
});
const analysisMetaSchema = z.strictObject({
  label_key: wireString,
  granularity: granularitySchema,
  join: joinTagSchema,
  status: viewStatusSchema,
  unavailable_reason: z.string().nullable().optional(),
  inputs: z.array(wireString),
  window: analysisWindowSchema,
  bound: pageBoundSchema,
});
function analysisSchema<T extends z.ZodType>(row: T) {
  return z.strictObject({ meta: analysisMetaSchema, data: pageSchema(row) });
}

const dependencyTopologySchema = z.strictObject({
  meta: analysisMetaSchema,
  modules: pageSchema(z.strictObject({
    module_id: wireString,
    fan_in: z.number().int().nonnegative(),
    fan_out: z.number().int().nonnegative(),
    cycle_ids: z.array(wireString),
  })),
  cycles: pageSchema(z.strictObject({ id: wireString, module_ids: z.array(wireString) })),
});
const hotspotSchema = analysisSchema(z.strictObject({
  module_id: wireString,
  commit_count: z.number().int().nonnegative(),
  fan_in: z.number().int().nonnegative(),
  quadrant: z.enum([
    "high_churn_high_fan_in",
    "high_churn_low_fan_in",
    "low_churn_high_fan_in",
    "low_churn_low_fan_in",
  ]),
}));
const hiddenCouplingSchema = analysisSchema(z.strictObject({
  left_module_id: wireString,
  right_module_id: wireString,
  cochange_count: z.number().int().nonnegative(),
  support: z.number().min(0).max(1),
}));
const treemapSchema = analysisSchema(z.strictObject({
  package_id: wireString,
  module_id: wireString,
  source_file_count: z.number().int().nonnegative(),
  recent_commit_count: availabilitySchema(z.number().int().nonnegative()),
}));
const cadencePointSchema = z.strictObject({
  week_start: z.string().regex(/^\d{4}-\d{2}-\d{2}$/),
  count: z.number().int().nonnegative(),
});
const cadenceSchema = z.strictObject({
  meta: analysisMetaSchema,
  commits: pageSchema(cadencePointSchema),
  issues_opened: pageSchema(cadencePointSchema),
  issues_closed: pageSchema(cadencePointSchema),
  pull_requests_opened: pageSchema(cadencePointSchema),
  pull_requests_merged: pageSchema(cadencePointSchema),
  release_tags: pageSchema(z.strictObject({
    name: wireString,
    target_sha: sha,
    committed_at: availabilitySchema(timestamp),
  })),
  pull_request_lead_time_hours: availabilitySchema(z.strictObject({
    p50: z.number().nonnegative(),
    p90: z.number().nonnegative(),
    p95: z.number().nonnegative(),
  })),
});
const authorShareSchema = z.strictObject({
  author: wireString,
  commits: z.number().int().nonnegative(),
  share: z.number().min(0).max(1),
});
const ownershipRowSchema = z.strictObject({
  module_id: wireString,
  commit_count: z.number().int().nonnegative(),
  author_concentration: availabilitySchema(z.number().min(0).max(1)),
  bus_factor: availabilitySchema(z.number().int().nonnegative()),
  authors: pageSchema(authorShareSchema),
});
const ownershipSchema = z.strictObject({
  meta: analysisMetaSchema,
  modules: pageSchema(ownershipRowSchema),
  repository_author_concentration: availabilitySchema(z.number().min(0).max(1)),
  repository_bus_factor: availabilitySchema(z.number().int().nonnegative()),
  repository_authors: pageSchema(authorShareSchema),
});
const apiSurfaceSchema = analysisSchema(z.strictObject({
  module_id: wireString,
  dependent_count: z.number().int().nonnegative(),
}));
const scorecardValueSchema = z.discriminatedUnion("value_kind", [
  z.strictObject({ value_kind: z.literal("count"), value: z.number().int().nonnegative() }),
  z.strictObject({ value_kind: z.literal("ratio"), value: z.number() }),
  z.strictObject({ value_kind: z.literal("text"), value: z.string() }),
  z.strictObject({ value_kind: z.literal("module_ids"), value: pageSchema(wireString) }),
]);
const scorecardSchema = z.strictObject({
  meta: analysisMetaSchema,
  fields: z.array(z.strictObject({
    key: z.enum([
      "repository_age_days",
      "package_count",
      "module_count",
      "symbol_count",
      "activity_trend",
      "top_hotspots",
      "dependency_cycle_count",
      "ownership_warnings",
    ]),
    label_key: wireString,
    granularity: granularitySchema,
    join: joinTagSchema,
    value: availabilitySchema(scorecardValueSchema),
  })),
});

export const repoBundleSchema = z.strictObject({
  schema_version: z.literal("khive.repo.v1"),
  meta: bundleMetaSchema,
  graph: repoGraphSchema,
  aggregates: z.strictObject({
    dependency_topology: dependencyTopologySchema,
    hotspot_quadrant: hotspotSchema,
    hidden_coupling: hiddenCouplingSchema,
    structure_treemap: treemapSchema,
    cadence_timeline: cadenceSchema,
    ownership: ownershipSchema,
    api_surface: apiSurfaceSchema,
    scorecard: scorecardSchema,
  }),
  capability: capabilitySchema,
}).superRefine((bundle, context) => {
  const repositoryIssue = publicRepositoryUrlIssue(
    bundle.meta.repository.canonical_url,
  );
  if (repositoryIssue) {
    context.addIssue({
      code: "custom",
      path: ["meta", "repository", "canonical_url"],
      message: repositoryIssue,
    });
  }
  if (bundle.meta.ingest.code_ingest.status === "available" &&
      bundle.meta.snapshot.head_sha !== bundle.meta.ingest.code_ingest.value.source_revision) {
    context.addIssue({ code: "custom", path: ["meta", "ingest", "code_ingest", "source_revision"], message: "code map revision must equal the bundle HEAD" });
  }
  for (const key of ["functions", "datatypes", "interfaces"] as const) {
    if (bundle.graph[key].items.length !== 0) {
      context.addIssue({ code: "custom", path: ["graph", key, "items"], message: "symbol-tier collections are typed but empty in khive.repo.v1" });
    }
  }
  const sourcePaths = new Set<string>();
  for (const [index, moduleNode] of bundle.graph.modules.items.entries()) {
    const pathIssue = addressableModulePathIssue(moduleNode.source_path);
    if (pathIssue) {
      context.addIssue({
        code: "custom",
        path: ["graph", "modules", "items", index, "source_path"],
        message: pathIssue,
      });
    }
    if (sourcePaths.has(moduleNode.source_path)) {
      context.addIssue({
        code: "custom",
        path: ["graph", "modules", "items", index, "source_path"],
        message: "module source paths must be unique within a repository snapshot",
      });
    }
    sourcePaths.add(moduleNode.source_path);
  }
  for (const key of [
    "dependency_topology",
    "hotspot_quadrant",
    "hidden_coupling",
    "structure_treemap",
    "cadence_timeline",
    "ownership",
    "api_surface",
    "scorecard",
  ] as const) {
    const allowedPartialOwnership = key === "ownership" &&
      bundle.capability.views.ownership.status === "unavailable" &&
      bundle.aggregates.ownership.meta.status === "available" &&
      bundle.aggregates.ownership.modules.disclosure.status === "unavailable";
    if (allowedPartialOwnership) continue;
    if (bundle.capability.views[key].status !== bundle.aggregates[key].meta.status) {
      context.addIssue({ code: "custom", path: ["capability", "views", key, "status"], message: "view capability status must match aggregate analysis status" });
    }
  }
  const singleDataPageKeys = [
    "hotspot_quadrant",
    "hidden_coupling",
    "structure_treemap",
    "api_surface",
  ] as const;
  for (const key of singleDataPageKeys) {
    const aggregate = bundle.aggregates[key];
    if (
      aggregate.meta.status === "unavailable" &&
      aggregate.data.disclosure.status !== "unavailable"
    ) {
      context.addIssue({
        code: "custom",
        path: ["aggregates", key, "data", "disclosure", "status"],
        message: "an unavailable aggregate analysis cannot retain a disclosed data page",
      });
    }
  }
  if (bundle.aggregates.dependency_topology.meta.status === "unavailable") {
    for (const pageKey of ["modules", "cycles"] as const) {
      if (bundle.aggregates.dependency_topology[pageKey].disclosure.status !== "unavailable") {
        context.addIssue({
          code: "custom",
          path: ["aggregates", "dependency_topology", pageKey, "disclosure", "status"],
          message: "an unavailable aggregate analysis cannot retain a disclosed data page",
        });
      }
    }
  }
  if (
    bundle.aggregates.ownership.meta.status === "unavailable" &&
    bundle.aggregates.ownership.modules.disclosure.status !== "unavailable"
  ) {
    context.addIssue({
      code: "custom",
      path: ["aggregates", "ownership", "modules", "disclosure", "status"],
      message: "an unavailable aggregate analysis cannot retain a disclosed data page",
    });
  }
});

export type RepoBundle = z.infer<typeof repoBundleSchema>;
export type RepoPage<T> = {
  items: T[];
  total_count: { status: "available"; value: number } | { status: "unavailable"; reason: string };
  bound: z.infer<typeof pageBoundSchema>;
  next_cursor?: string | null;
  truncated: boolean;
  disclosure: z.infer<typeof disclosureSchema>;
};
export type RepoModule = RepoBundle["graph"]["modules"]["items"][number];
export type RepoCommit = RepoBundle["graph"]["commits"]["items"][number];
export type ViewId = keyof RepoBundle["capability"]["views"];

export function parseRepoBundle(value: unknown): RepoBundle {
  return repoBundleSchema.parse(value);
}
