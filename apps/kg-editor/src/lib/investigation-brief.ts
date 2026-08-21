import type { ShowcaseBundleSource } from "@/lib/adapters/preferred-showcase-source";
import { buildModuleInsight } from "@/lib/repository-brief";
import type { RepoBundle, ViewId } from "@/lib/repo-bundle";
import {
  publicRepositoryUrlIssue,
  type StructureGraphLocation,
} from "@/lib/repository-location";
import { structureCouplingPairKey } from "@/lib/structure-coupling-lens";

export const INVESTIGATION_BRIEF_MAX_CHARS = 48 * 1_024;
export const INVESTIGATION_BRIEF_VERIFY_INSTRUCTION =
  "Verify at the recorded full SHA: inspect the named source paths and direct dependencies, then confirm or refute each candidate with code and history evidence. Do not treat this brief as a defect claim.";

const INLINE_VALUE_LIMIT = 320;
const PATH_VALUE_LIMIT = 1_100;
const REPOSITORY_URL_VALUE_LIMIT = 2_048;
const CURRENT_URL_VALUE_LIMIT = 12_288;
const CODE_SPAN_OVERHEAD = 48;
const RECENT_COMMIT_LIMIT = 5;
const OWNERSHIP_AUTHOR_LIMIT = 5;
const SCC_LIMIT = 3;
const SCC_MEMBER_LIMIT = 6;
const AUTHOR_TOKEN_HEX_CHARS = 10;

// Model-facing dynamic-field allowlist.
//
// The brief below is copied verbatim into an instruction-following model's
// context, so every dynamic value it contains must fall into one of these
// bounded classes — nothing else is permitted through untouched. Markdown
// escaping/truncation (`code()`/`boundedInline()`) is presentation only; it
// is never treated as the instruction/data boundary by itself. The boundary
// is enforced at the source contract, in `repo-bundle.ts`:
//
//   1. Constrained identifiers: full/short commit SHAs, ISO-8601 timestamps,
//      and numeric stats (counts, percentages, weights) are regex/type
//      validated in the bundle schema and cannot carry instruction text by
//      construction.
//   2. Validated repository-relative source/module paths (`source_path`,
//      `module_path`) and a closed `language` enum — all rejected at parse
//      if they fall outside the contract; rendered through `code()` for a
//      hard length bound plus escaping on top.
//   3. Bounded identifier tokens (`producer.exporter`, SCC `cycle.id`),
//      closed enums (`bound.order`), and opaque cursor tokens
//      (`next_cursor`) — all schema-validated closed/bounded contracts in
//      `repo-bundle.ts`, not free text.
//   4. Producer-authored disclosure/unavailable-reason text has no stable
//      status-code vocabulary in the khive exporter today, so it remains
//      free text — residual risk the schema cannot close by enum. The
//      schema still bounds length and rejects control characters
//      (`reasonText` in `repo-bundle.ts`), and this module additionally
//      renders every value through `code()`, a delimited Markdown code
//      span, as a second, presentation-layer control.
//   5. Repository-controlled free text (commit subjects, commit and
//      ownership author identities) is NEVER copied verbatim. Commit
//      subjects are omitted from the brief entirely. Author identities are
//      replaced by `authorToken()`, a short stable hash labeled as a hashed
//      token so no attacker-supplied identity text reaches the model
//      channel.
//   6. The caller-supplied `canonicalUrl` is validated at runtime with
//      `publicRepositoryUrlIssue` (the same public-HTTP(S)-URL contract
//      used for the bundle's own repository URL) before use; a failing
//      value renders a bounded placeholder, never the raw string.
//
// Adding a new dynamic field to the brief means placing it in class 1-4
// above, or hashing/dropping/validating it per class 5-6 — never passing an
// unconstrained repository- or caller-sourced string through untouched.

// FNV-1a 32-bit, extended by re-hashing the running state until the token is
// long enough. Deterministic across runtimes so the same author collapses to
// the same token every time the brief is rebuilt.
export function authorToken(value: string): string {
  let state = 0x811c9dc5;
  for (let index = 0; index < value.length; index += 1) {
    state ^= value.charCodeAt(index);
    state = Math.imul(state, 0x01000193);
  }
  let token = "";
  let extended = state >>> 0;
  while (token.length < AUTHOR_TOKEN_HEX_CHARS) {
    token += extended.toString(16).padStart(8, "0");
    extended = Math.imul(extended ^ 0x9e3779b9, 0x01000193) >>> 0;
  }
  return token.slice(0, AUTHOR_TOKEN_HEX_CHARS);
}

type BoundedPage = Readonly<{
  items: readonly unknown[];
  total_count:
    | Readonly<{ status: "available"; value: number }>
    | Readonly<{ status: "unavailable"; reason: string }>;
  bound: Readonly<{
    kind: "all" | "top_n";
    max_items: number;
    order: string;
  }>;
  next_cursor?: string | null;
  truncated: boolean;
  disclosure: Readonly<{
    status: "complete" | "truncated" | "unavailable";
    reason?: string | null;
  }>;
}>;

export type InvestigationBriefInput = Readonly<{
  bundle: RepoBundle;
  analysisSource: ShowcaseBundleSource;
  canonicalUrl: string;
  activeView: ViewId;
  selectedModuleId: string;
  structureGraph: StructureGraphLocation;
}>;

export type InvestigationBriefErrorCode =
  | "selected_module_revision_mismatch"
  | "focused_pair_revision_mismatch"
  | "referenced_module_revision_mismatch";

export class InvestigationBriefError extends Error {
  readonly code: InvestigationBriefErrorCode;

  constructor(code: InvestigationBriefErrorCode, message: string) {
    super(message);
    this.name = "InvestigationBriefError";
    this.code = code;
  }
}

const TRUNCATION_SUFFIX = "… [truncated]";

function normalizedInline(value: string): string {
  return value
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/gu, "�")
    .replace(/\r\n?/gu, "\n")
    .replace(/\s+/gu, " ")
    .trim();
}

function truncateInline(value: string, limit: number): string {
  if (value.length <= limit) return value;
  if (limit <= TRUNCATION_SUFFIX.length) {
    return TRUNCATION_SUFFIX.slice(0, Math.max(0, limit));
  }
  return `${value.slice(0, limit - TRUNCATION_SUFFIX.length)}${TRUNCATION_SUFFIX}`;
}

function boundedInline(value: string, limit = INLINE_VALUE_LIMIT): string {
  return truncateInline(normalizedInline(value), limit);
}

export function markdownCodeSpan(value: string): string {
  const normalized = value.replace(/\r\n?/gu, "\n").replace(/\n/gu, " ");
  const runs = normalized.match(/`+/gu) ?? [];
  const delimiter = "`".repeat(
    Math.max(1, ...runs.map((run) => run.length + 1)),
  );
  return `${delimiter} ${normalized} ${delimiter}`;
}

function code(
  value: string,
  limit = INLINE_VALUE_LIMIT,
  renderedLimit = limit + CODE_SPAN_OVERHEAD,
): string {
  const normalized = normalizedInline(value);
  const initial = markdownCodeSpan(truncateInline(normalized, limit));
  if (initial.length <= renderedLimit) return initial;

  // A long contiguous backtick run makes a valid Markdown delimiter grow with
  // the value. Find the largest raw prefix whose *escaped* span still fits.
  let lower = 0;
  let upper = Math.min(limit, normalized.length);
  let bounded = markdownCodeSpan("");
  while (lower <= upper) {
    const midpoint = Math.floor((lower + upper) / 2);
    const candidate = markdownCodeSpan(truncateInline(normalized, midpoint));
    if (candidate.length <= renderedLimit) {
      bounded = candidate;
      lower = midpoint + 1;
    } else {
      upper = midpoint - 1;
    }
  }
  return bounded;
}

function pageIsComplete(page: BoundedPage): boolean {
  return page.disclosure.status === "complete" &&
    !page.truncated &&
    page.next_cursor == null;
}

function pageStatus(page: BoundedPage): "complete" | "truncated" | "unavailable" {
  if (page.disclosure.status === "unavailable") return "unavailable";
  return pageIsComplete(page) ? "complete" : "truncated";
}

function pageCoverage(
  label: string,
  page: BoundedPage,
  unavailableReason?: string | null,
): string {
  const status = pageStatus(page);
  const total = page.total_count.status === "available"
    ? `${page.total_count.value} declared`
    : `declared total unavailable (${code(page.total_count.reason)})`;
  const reasons = [
    unavailableReason,
    page.disclosure.reason,
    page.next_cursor != null
      ? `continuation cursor ${boundedInline(page.next_cursor)}`
      : null,
    page.truncated && page.disclosure.reason == null
      ? "the captured page is marked truncated"
      : null,
  ].filter((reason): reason is string => Boolean(reason));
  return `- ${label}: **${status}**; ${page.items.length} captured, ${total}; bound ${code(page.bound.kind)} to ${page.bound.max_items}, ordered by ${code(page.bound.order)}${
    reasons.length > 0 ? `; ${reasons.map((reason) => code(reason)).join("; ")}` : ""
  }.`;
}

function unavailableCoverage(label: string, reason: string): string {
  return `- ${label}: **unavailable**; ${code(reason)}.`;
}

function formatWindow(
  window: RepoBundle["aggregates"]["hidden_coupling"]["meta"]["window"],
): string {
  if (window.kind === "rolling_days") {
    const duration = window.days == null ? "Rolling" : `${window.days}-day`;
    const range = window.start && window.end
      ? ` (${boundedInline(window.start)} to ${boundedInline(window.end)})`
      : "";
    return `${duration} analysis window${range}`;
  }
  if (window.kind === "range") {
    return `Bounded analysis range ${boundedInline(window.start ?? "unspecified start")} to ${boundedInline(window.end ?? "unspecified end")}`;
  }
  return "Declared all-history analysis window";
}

function percentage(value: number): string {
  return `${(value * 100).toFixed(1)}%`;
}

function sourceDescription(source: ShowcaseBundleSource): string {
  return source === "khive-db-snapshot"
    ? "Materialized khive DB snapshot"
    : "Curated static fallback bundle";
}

const CANONICAL_URL_PLACEHOLDER = "unavailable — invalid canonical URL";

function boundedCanonicalUrl(value: string): string {
  return publicRepositoryUrlIssue(value) === null
    ? code(value, CURRENT_URL_VALUE_LIMIT)
    : code(CANONICAL_URL_PLACEHOLDER);
}

function appendOptionalBlocks(
  core: string,
  blocks: readonly string[],
  footer: string,
): string {
  for (let included = blocks.length; included >= 0; included -= 1) {
    const omitted = blocks.length - included;
    const status = omitted === 0 ? "complete" : "truncated";
    const disclosure =
      `- Optional detail coverage: **${status}**; ${omitted} bounded detail block${omitted === 1 ? " was" : "s were"} omitted${omitted === 0 ? "." : ` to honor the ${INVESTIGATION_BRIEF_MAX_CHARS}-character brief limit.`}`;
    const optional = blocks.slice(0, included).join("\n\n");
    const result = [core, optional, disclosure, footer]
      .filter(Boolean)
      .join("\n\n") + "\n";
    if (result.length <= INVESTIGATION_BRIEF_MAX_CHARS) return result;
  }

  // Dynamic fields are escaped with rendered-size budgets, so a valid bundle
  // cannot normally reach this path. Keep the public model total even if a
  // future mandatory section grows without updating those budgets: export no
  // partial claim, reserve the omission disclosure and verification footer.
  const fallback = [
    "# Bounded repository investigation brief",
    "",
    "## Evidence encoding coverage",
    "",
    "- Mandatory evidence is **unavailable** because it exceeded the declared output bound; no repository finding was exported.",
    `- Optional detail coverage: **truncated**; ${blocks.length} bounded detail block${blocks.length === 1 ? " was" : "s were"} omitted to honor the ${INVESTIGATION_BRIEF_MAX_CHARS}-character brief limit.`,
    "",
    footer,
    "",
  ].join("\n");
  return fallback.slice(0, INVESTIGATION_BRIEF_MAX_CHARS);
}

export function buildInvestigationBrief({
  bundle,
  analysisSource,
  canonicalUrl,
  activeView,
  selectedModuleId,
  structureGraph,
}: InvestigationBriefInput): string | null {
  const insight = buildModuleInsight(bundle, selectedModuleId);
  if (!insight) return null;
  if (insight.module.source_revision !== bundle.meta.snapshot.head_sha) {
    throw new InvestigationBriefError(
      "selected_module_revision_mismatch",
      `Selected module revision does not match the recorded snapshot SHA (${insight.module.source_path}).`,
    );
  }

  const moduleBySourcePath = new Map<string, RepoBundle["graph"]["modules"]["items"]>();
  for (const moduleNode of bundle.graph.modules.items) {
    const matches = moduleBySourcePath.get(moduleNode.source_path) ?? [];
    matches.push(moduleNode);
    moduleBySourcePath.set(moduleNode.source_path, matches);
  }
  const historyRow = bundle.graph.history_navigation.by_module.items.find(
    (row) => row.module_id === selectedModuleId,
  );
  const ownershipRow = bundle.aggregates.ownership.meta.status === "available"
    ? bundle.aggregates.ownership.modules.items.find(
      (row) => row.module_id === selectedModuleId,
    )
    : undefined;

  const focusedPaths = activeView === "structure_graph" &&
      structureGraph.lens === "hidden_coupling"
    ? structureGraph.couplingPair
    : null;
  const focusedModules = focusedPaths?.map((path) => {
    const matches = moduleBySourcePath.get(path) ?? [];
    return matches.length === 1 ? matches[0] : null;
  }) ?? [];
  const focusedPairKey = focusedModules.length === 2 &&
      focusedModules[0] && focusedModules[1]
    ? structureCouplingPairKey(focusedModules[0].id, focusedModules[1].id)
    : null;
  if (
    focusedModules.some((moduleNode) =>
      moduleNode != null &&
      moduleNode.source_revision !== bundle.meta.snapshot.head_sha
    )
  ) {
    throw new InvestigationBriefError(
      "focused_pair_revision_mismatch",
      "Focused pair endpoint revision does not match the recorded snapshot SHA.",
    );
  }
  const mismatchedModule = bundle.graph.modules.items.find((moduleNode) =>
    moduleNode.source_revision !== bundle.meta.snapshot.head_sha
  );
  if (mismatchedModule) {
    throw new InvestigationBriefError(
      "referenced_module_revision_mismatch",
      `Captured module revision does not match the recorded snapshot SHA (${mismatchedModule.source_path}).`,
    );
  }
  const focusedPair = focusedPairKey &&
      bundle.aggregates.hidden_coupling.meta.status === "available"
    ? bundle.aggregates.hidden_coupling.data.items.find((pair) =>
      structureCouplingPairKey(pair.left_module_id, pair.right_module_id) ===
        focusedPairKey
    )
    : undefined;
  const capturedDirectEdge = focusedPairKey
    ? bundle.graph.structure_edges.items.some((edge) =>
      edge.relation === "depends_on" &&
      structureCouplingPairKey(edge.source, edge.target) === focusedPairKey
    )
    : false;

  const sccSummary = insight.topology.cycles.length > 0
    ? `**Observed** SCC membership: ${insight.topology.cycles.length} captured SCC${insight.topology.cycles.length === 1 ? "" : "s"}.`
    : pageIsComplete(bundle.aggregates.dependency_topology.cycles) &&
        insight.topology.coverage.status === "complete"
    ? "**Observed** SCC membership: no captured SCC contains this module."
    : "SCC membership is **unknown beyond captured rows** because topology coverage is incomplete or unavailable.";
  const ownershipSummary = ownershipRow
    ? `**Observed ownership evidence**: ${ownershipRow.commit_count} captured commits; author concentration ${
      ownershipRow.author_concentration.status === "available"
        ? percentage(ownershipRow.author_concentration.value)
        : "unavailable"
    }; bus factor ${
      ownershipRow.bus_factor.status === "available"
        ? ownershipRow.bus_factor.value
        : "unavailable"
    }.`
    : `Ownership evidence is **unavailable for this module**; ${code(
      bundle.aggregates.ownership.meta.unavailable_reason ??
        "no captured ownership row resolves to the selected module",
    )}.`;

  const lines = [
    "# Bounded repository investigation brief",
    "",
    "## Provenance",
    "",
    `- Repository: ${code(bundle.meta.repository.canonical_url, REPOSITORY_URL_VALUE_LIMIT)}.`,
    `- Evidence source: **${sourceDescription(analysisSource)}** — captured evidence, not a live repository query.`,
    `- Snapshot full SHA: ${code(bundle.meta.snapshot.head_sha)}.`,
    `- Snapshot captured at: ${code(bundle.meta.snapshot.ingested_at)}.`,
    `- Exporter: ${code(bundle.meta.producer.exporter)}.`,
    `- Module revision binding: all ${bundle.graph.modules.items.length} captured module rows match the recorded snapshot full SHA.`,
    `- Canonical current URL: ${boundedCanonicalUrl(canonicalUrl)}.`,
    "",
    "## Selected module",
    "",
    "- Classification: **Observed module evidence** within the bounded capture; ranks and signals are not defect claims.",
    `- Source path: ${code(insight.module.source_path, PATH_VALUE_LIMIT)}.`,
    `- Module source revision: ${code(insight.module.source_revision)}; matches the recorded snapshot full SHA.`,
    `- Module path: ${code(insight.module.module_path, PATH_VALUE_LIMIT)}; language ${code(insight.module.language)}.`,
    `- Captured topology metrics: fan-in ${insight.topology.coverage.status === "unavailable" ? "unavailable" : insight.topology.fanIn}; fan-out ${insight.topology.coverage.status === "unavailable" ? "unavailable" : insight.topology.fanOut}.`,
    `- ${sccSummary}`,
    `- Captured history: ${insight.history.status === "unavailable" ? "unavailable" : `${insight.history.shown} module-linked commit IDs${insight.history.total == null ? "" : ` of ${insight.history.total} declared`}`}.`,
    `- ${ownershipSummary}`,
    insight.hotspot
      ? `- **Candidate hotspot**, not an observed defect: ${insight.hotspot.commitCount} captured commits, fan-in ${insight.hotspot.fanIn}, quadrant ${code(insight.hotspot.quadrant)}.`
      : "- Hotspot candidate: unavailable for this selected module.",
    "",
    "## Focused pair",
    "",
  ];

  if (focusedPaths && focusedPaths.length === 2) {
    lines.push(
      "- Classification: **Candidate hidden coupling**, not a dependency or defect claim.",
      `- Endpoints: ${code(focusedPaths[0], PATH_VALUE_LIMIT)} and ${code(focusedPaths[1], PATH_VALUE_LIMIT)}.`,
      focusedModules.length === 2 && focusedModules[0] && focusedModules[1]
        ? `- Endpoint source revisions: ${code(focusedModules[0].source_revision)} and ${code(focusedModules[1].source_revision)}; both match the recorded snapshot full SHA.`
        : "- Endpoint source revision binding is unavailable because the focused paths do not resolve to two unique captured modules.",
      focusedPair
        ? `- Observed co-change evidence: ${focusedPair.cochange_count} co-changes; ${percentage(focusedPair.support)} support. ${formatWindow(bundle.aggregates.hidden_coupling.meta.window)}.`
        : "- Co-change/support evidence is unavailable because the focused paths do not resolve to one captured pair.",
    );
    if (!focusedPairKey) {
      lines.push(
        "- Direct-edge evidence is unknown because the focused paths do not resolve to two unique captured modules.",
      );
    } else if (capturedDirectEdge) {
      lines.push(
        "- Observed direct-edge evidence: a captured direct dependency edge is present between the endpoints.",
      );
    } else if (pageIsComplete(bundle.graph.structure_edges)) {
      lines.push(
        "- Observed direct-edge evidence: No captured direct dependency edge exists between the endpoints in the complete structure-edge page.",
      );
    } else {
      lines.push(
        "- Direct-edge evidence is unknown because structure-edge coverage is incomplete; absence is not inferred.",
      );
    }
  } else {
    lines.push(
      "- No focused hidden-coupling pair is encoded in the current structure location.",
    );
  }

  lines.push(
    "",
    "## Coverage and interpretation caveats",
    "",
    `- Dependency topology window: ${formatWindow(bundle.aggregates.dependency_topology.meta.window)}.`,
    `- Hotspot window: ${formatWindow(bundle.aggregates.hotspot_quadrant.meta.window)}.`,
    `- Ownership window: ${formatWindow(bundle.aggregates.ownership.meta.window)}.`,
    `- Hidden-coupling window: ${formatWindow(bundle.aggregates.hidden_coupling.meta.window)}.`,
    pageCoverage("Module-page coverage", bundle.graph.modules),
    pageCoverage(
      "Topology-module coverage",
      bundle.aggregates.dependency_topology.modules,
      bundle.aggregates.dependency_topology.meta.status === "unavailable"
        ? bundle.aggregates.dependency_topology.meta.unavailable_reason
        : null,
    ),
    pageCoverage(
      "SCC-page coverage",
      bundle.aggregates.dependency_topology.cycles,
      bundle.aggregates.dependency_topology.meta.status === "unavailable"
        ? bundle.aggregates.dependency_topology.meta.unavailable_reason
        : null,
    ),
    pageCoverage("Structure-edge coverage", bundle.graph.structure_edges),
    pageCoverage(
      "Module-history-index coverage",
      bundle.graph.history_navigation.by_module,
    ),
    historyRow
      ? pageCoverage("Selected-module history coverage", historyRow.commits)
      : unavailableCoverage(
        "Selected-module history coverage",
        "no captured module history-navigation row",
      ),
    pageCoverage("Commit-record coverage", bundle.graph.commits),
    pageCoverage(
      "Hotspot coverage",
      bundle.aggregates.hotspot_quadrant.data,
      bundle.aggregates.hotspot_quadrant.meta.status === "unavailable"
        ? bundle.aggregates.hotspot_quadrant.meta.unavailable_reason
        : null,
    ),
    pageCoverage(
      "Ownership-module coverage",
      bundle.aggregates.ownership.modules,
      bundle.aggregates.ownership.meta.status === "unavailable"
        ? bundle.aggregates.ownership.meta.unavailable_reason
        : null,
    ),
    ownershipRow
      ? pageCoverage("Selected-module author coverage", ownershipRow.authors)
      : unavailableCoverage(
        "Selected-module author coverage",
        "no captured ownership row resolves to the selected module",
      ),
    pageCoverage(
      "Hidden-coupling coverage",
      bundle.aggregates.hidden_coupling.data,
      bundle.aggregates.hidden_coupling.meta.status === "unavailable"
        ? bundle.aggregates.hidden_coupling.meta.unavailable_reason
        : null,
    ),
    "- Source-role caveat: captured module rows can include production, test, example, and generated sources; verify each path before interpreting architecture or rank.",
    "- Candidate vs observed: counts, captured edges, commits, SCC rows, and ownership rows are observations of this bounded capture; hotspot and hidden-coupling interpretations remain candidates until code/history inspection confirms or refutes them.",
    `- Detail bounds: recent commits are capped at ${RECENT_COMMIT_LIMIT}, ownership authors at ${OWNERSHIP_AUTHOR_LIMIT}, SCCs at ${SCC_LIMIT}, and members per SCC at ${SCC_MEMBER_LIMIT}; the whole Markdown brief is capped at ${INVESTIGATION_BRIEF_MAX_CHARS} characters.`,
  );

  const optionalBlocks: string[] = [];
  if (insight.topology.cycles.length > 0) {
    const cycleLines = insight.topology.cycles.slice(0, SCC_LIMIT).map((cycle) => {
      const members = cycle.modules.slice(0, SCC_MEMBER_LIMIT).map((moduleNode) =>
        code(moduleNode.source_path, PATH_VALUE_LIMIT)
      );
      const omitted = Math.max(0, cycle.moduleIds.length - members.length);
      return `- ${code(cycle.id)}: ${members.join(", ")}${omitted > 0 ? `; ${omitted} additional captured member${omitted === 1 ? "" : "s"} omitted` : ""}.`;
    });
    optionalBlocks.push(`## Captured SCC records\n\n${cycleLines.join("\n")}`);
  }
  if (ownershipRow) {
    const authorLines = ownershipRow.authors.items
      .slice(0, OWNERSHIP_AUTHOR_LIMIT)
      .map((author) =>
        `- author token ${code(authorToken(author.author))} (hashed identity, not raw text): ${author.commits} commits; ${percentage(author.share)} share.`
      );
    if (authorLines.length > 0) {
      optionalBlocks.push(
        `## Captured ownership records\n\n${authorLines.join("\n")}`,
      );
    }
  }
  if (insight.recentCommits.length > 0) {
    const commitLines = insight.recentCommits
      .slice(0, RECENT_COMMIT_LIMIT)
      .map((commit) =>
        `- ${code(commit.sha)} at ${code(commit.committed_at)}, author token ${
          code(authorToken(commit.author))
        } (hashed identity, not raw text).`
      );
    optionalBlocks.push(
      `## Captured recent history records\n\n${commitLines.join("\n")}`,
    );
  }

  const footer =
    `## Verify / confirm or refute\n\n${INVESTIGATION_BRIEF_VERIFY_INSTRUCTION}`;
  return appendOptionalBlocks(lines.join("\n"), optionalBlocks, footer);
}
