import type { RepoBundle, RepoCommit, RepoModule } from "@/lib/repo-bundle";
import { structureCouplingPairKey } from "@/lib/structure-coupling-lens";

export const COUPLING_SHARED_COMMIT_LIMIT = 5;
export const COUPLING_COMMON_NEIGHBOR_LIMIT = 6;
export const COUPLING_CYCLE_LIMIT = 3;
export const COUPLING_SCC_MEMBER_LIMIT = 6;
export const COUPLING_OWNERSHIP_AUTHOR_LIMIT = 3;
export const COUPLING_DIRECT_DEPENDENCY_LIMIT = 2;

type PageLike = Readonly<{
  items: readonly unknown[];
  total_count:
    | Readonly<{ status: "available"; value: number }>
    | Readonly<{ status: "unavailable"; reason: string }>;
  bound: Readonly<{ max_items: number }>;
  next_cursor?: string | null;
  truncated: boolean;
  disclosure: Readonly<{
    status: "complete" | "truncated" | "unavailable";
    reason?: string | null;
  }>;
}>;

type TopologyRow =
  RepoBundle["aggregates"]["dependency_topology"]["modules"]["items"][number];
type CycleRow =
  RepoBundle["aggregates"]["dependency_topology"]["cycles"]["items"][number];
type HotspotRow =
  RepoBundle["aggregates"]["hotspot_quadrant"]["data"]["items"][number];
type HistoryRow =
  RepoBundle["graph"]["history_navigation"]["by_module"]["items"][number];
type OwnershipRow =
  RepoBundle["aggregates"]["ownership"]["modules"]["items"][number];
export type CouplingAnalysisWindow =
  RepoBundle["aggregates"]["hidden_coupling"]["meta"]["window"];

export type CouplingEvidenceState = "present" | "absent" | "unknown";
export type CouplingCoverageStatus =
  | "complete"
  | "truncated"
  | "unavailable";

export type CouplingEvidenceBoundary = Readonly<{
  status: CouplingCoverageStatus;
  shown: number;
  declared: number | null;
  bound: number;
  reason: string | null;
}>;

export type CouplingTopologyEvidence = Readonly<{
  state: CouplingEvidenceState;
  fanIn: number | null;
  fanOut: number | null;
  boundary: CouplingEvidenceBoundary;
}>;

export type CouplingSccEvidence = Readonly<{
  state: CouplingEvidenceState;
  items: readonly Readonly<{
    id: string;
    modules: readonly RepoModule[];
    memberBoundary: CouplingEvidenceBoundary;
  }>[];
  boundary: CouplingEvidenceBoundary;
}>;

export type CouplingHotspotEvidence = Readonly<{
  state: CouplingEvidenceState;
  commitCount: number | null;
  fanIn: number | null;
  quadrant: HotspotRow["quadrant"] | null;
  rank: number | null;
  window: CouplingAnalysisWindow;
  boundary: CouplingEvidenceBoundary;
}>;

export type CouplingHistoryEvidence = Readonly<{
  state: CouplingEvidenceState;
  boundary: CouplingEvidenceBoundary;
}>;

export const CAPTURED_OWNERSHIP_CAVEAT =
  "Captured contribution history does not establish current ownership, availability, or review responsibility.";

export type CouplingOwnershipEvidence = Readonly<{
  state: CouplingEvidenceState;
  commitCount: number | null;
  authorConcentration: number | null;
  busFactor: number | null;
  authors: readonly OwnershipRow["authors"]["items"][number][];
  window: CouplingAnalysisWindow;
  boundary: CouplingEvidenceBoundary;
  caveat: string;
}>;

export type CouplingEndpointEvidence = Readonly<{
  module: RepoModule;
  topology: CouplingTopologyEvidence;
  scc: CouplingSccEvidence;
  hotspot: CouplingHotspotEvidence;
  history: CouplingHistoryEvidence;
  ownership: CouplingOwnershipEvidence;
}>;

export type CouplingSharedCommit = Readonly<{
  id: string;
  commit: RepoCommit;
}>;

export type CouplingSharedCommitEvidence = Readonly<{
  state: CouplingEvidenceState;
  items: readonly CouplingSharedCommit[];
  boundary: CouplingEvidenceBoundary;
}>;

export type CouplingNeighborDirection = "incoming" | "outgoing";

export type CouplingCommonNeighbor = Readonly<{
  module: RepoModule;
  leftDirection: CouplingNeighborDirection;
  leftRelation: string;
  rightDirection: CouplingNeighborDirection;
  rightRelation: string;
}>;

export type CouplingCommonNeighborEvidence = Readonly<{
  state: CouplingEvidenceState;
  items: readonly CouplingCommonNeighbor[];
  boundary: CouplingEvidenceBoundary;
}>;

export type CouplingDependencyDirection =
  | "left_to_right"
  | "right_to_left";

export type CouplingDirectDependencyEvidence = Readonly<{
  state: CouplingEvidenceState;
  directions: readonly CouplingDependencyDirection[];
  boundary: CouplingEvidenceBoundary;
}>;

export type CouplingComparison = Readonly<{
  sourceRevision: string;
  endpoints: readonly [CouplingEndpointEvidence, CouplingEndpointEvidence];
  cochange: Readonly<{
    state: "present";
    count: number;
    support: number;
    boundary: CouplingEvidenceBoundary;
  }>;
  sharedCommits: CouplingSharedCommitEvidence;
  commonNeighbors: CouplingCommonNeighborEvidence;
  directDependency: CouplingDirectDependencyEvidence;
  caveat: string;
  verifyPrompts: readonly string[];
}>;

export type CouplingComparisonUnavailableCode =
  | "pair_unresolved"
  | "pair_evidence_unavailable"
  | "pair_evidence_ambiguous"
  | "referenced_module_missing"
  | "source_revision_mismatch"
  | "evidence_revision_mismatch";

export type CouplingComparisonResult =
  | Readonly<{ status: "available"; value: CouplingComparison }>
  | Readonly<{
    status: "unavailable";
    code: CouplingComparisonUnavailableCode;
    reason: string;
  }>;

export function couplingComparisonResultStatus(
  result: CouplingComparisonResult,
): string {
  return result.status === "available"
    ? "Boundary evidence available"
    : `Boundary evidence unavailable · ${result.code}: ${result.reason}`;
}

export function couplingAnalysisWindowLabel(
  window: CouplingAnalysisWindow,
): string {
  if (window.kind === "rolling_days") {
    const duration = window.days == null ? "Rolling" : `${window.days}-day`;
    const range = window.start && window.end
      ? ` (${window.start} to ${window.end})`
      : "";
    return `${duration} analysis window${range}`;
  }
  if (window.kind === "range") {
    return `Bounded analysis range ${window.start ?? "unspecified start"} to ${window.end ?? "unspecified end"}`;
  }
  return "Declared all-history analysis window";
}

class BuildFailure extends Error {
  readonly code: CouplingComparisonUnavailableCode;

  constructor(code: CouplingComparisonUnavailableCode, message: string) {
    super(message);
    this.code = code;
  }
}

function compareText(left: string, right: string): number {
  if (left < right) return -1;
  if (left > right) return 1;
  return 0;
}

function pageStatus(page: PageLike): CouplingCoverageStatus {
  if (page.disclosure.status === "unavailable") return "unavailable";
  return page.disclosure.status === "truncated" ||
      page.truncated ||
      page.next_cursor != null
    ? "truncated"
    : "complete";
}

function pageReason(page: PageLike, fallback?: string | null): string | null {
  if (fallback) return fallback;
  if (page.disclosure.reason) return page.disclosure.reason;
  if (page.next_cursor != null) {
    return "The captured source page has a continuation cursor.";
  }
  if (page.truncated) return "The captured source page is truncated.";
  if (page.disclosure.status === "unavailable") {
    return page.total_count.status === "unavailable"
      ? page.total_count.reason
      : "The captured source page is unavailable.";
  }
  return null;
}

function weakestStatus(
  statuses: readonly CouplingCoverageStatus[],
): CouplingCoverageStatus {
  if (statuses.includes("unavailable")) return "unavailable";
  if (statuses.includes("truncated")) return "truncated";
  return "complete";
}

function combineReasons(
  ...reasons: readonly (string | null | undefined)[]
): string | null {
  const unique = [...new Set(reasons.filter(
    (reason): reason is string => Boolean(reason),
  ))];
  return unique.length > 0 ? unique.join("; ") : null;
}

function localBoundary({
  sourceStatuses,
  sourceReasons,
  shown,
  captured,
  declared,
  bound,
  localReason,
}: Readonly<{
  sourceStatuses: readonly CouplingCoverageStatus[];
  sourceReasons: readonly (string | null)[];
  shown: number;
  captured: number;
  declared: number | null;
  bound: number;
  localReason?: string | null;
}>): CouplingEvidenceBoundary {
  const sourceStatus = weakestStatus(sourceStatuses);
  const locallyTruncated = shown < captured;
  return {
    status: sourceStatus === "unavailable"
      ? "unavailable"
      : sourceStatus === "truncated" || locallyTruncated
      ? "truncated"
      : "complete",
    shown,
    declared,
    bound,
    reason: combineReasons(
      ...sourceReasons,
      locallyTruncated ? localReason : null,
    ),
  };
}

function pageBoundary(page: PageLike): CouplingEvidenceBoundary {
  return {
    status: pageStatus(page),
    shown: page.items.length,
    declared: page.total_count.status === "available"
      ? page.total_count.value
      : null,
    bound: page.bound.max_items,
    reason: pageReason(page),
  };
}

function evidenceState(
  itemCount: number,
  status: CouplingCoverageStatus,
): CouplingEvidenceState {
  if (itemCount > 0) return "present";
  return status === "complete" ? "absent" : "unknown";
}

function assertModuleRevision(
  moduleNode: RepoModule,
  revision: string,
): void {
  if (moduleNode.source_revision !== revision) {
    throw new BuildFailure(
      "source_revision_mismatch",
      `Referenced module revision does not match the recorded snapshot SHA (${moduleNode.source_path}).`,
    );
  }
}

function assertEdgeRevision(
  edge: RepoBundle["graph"]["structure_edges"]["items"][number],
  revision: string,
): void {
  const derivation = edge.derivation;
  if (
    derivation &&
    "source_revision" in derivation &&
    derivation.source_revision !== revision
  ) {
    throw new BuildFailure(
      "evidence_revision_mismatch",
      `Referenced structure evidence does not match the recorded snapshot SHA (${edge.id}).`,
    );
  }
}

function singleRow<T>(
  rows: readonly T[],
  message: string,
): T | undefined {
  if (rows.length > 1) {
    throw new BuildFailure("pair_evidence_ambiguous", message);
  }
  return rows[0];
}

function buildTopology(
  bundle: RepoBundle,
  moduleId: string,
): { evidence: CouplingTopologyEvidence; row: TopologyRow | undefined } {
  const analysis = bundle.aggregates.dependency_topology;
  if (analysis.meta.status === "unavailable") {
    const reason = analysis.meta.unavailable_reason ??
      "Dependency topology was not produced.";
    return {
      row: undefined,
      evidence: {
        state: "unknown",
        fanIn: null,
        fanOut: null,
        boundary: {
          status: "unavailable",
          shown: 0,
          declared: null,
          bound: 1,
          reason,
        },
      },
    };
  }
  const sourceStatus = pageStatus(analysis.modules);
  const row = singleRow(
    analysis.modules.items.filter((candidate) =>
      candidate.module_id === moduleId
    ),
    `Multiple dependency-topology rows resolve to ${moduleId}.`,
  );
  return {
    row,
    evidence: {
      state: row ? "present" : sourceStatus === "complete" ? "absent" : "unknown",
      fanIn: row?.fan_in ?? null,
      fanOut: row?.fan_out ?? null,
      boundary: localBoundary({
        sourceStatuses: [sourceStatus],
        sourceReasons: [pageReason(analysis.modules)],
        shown: row ? 1 : 0,
        captured: row ? 1 : 0,
        declared: sourceStatus === "complete" ? row ? 1 : 0 : null,
        bound: 1,
      }),
    },
  };
}

function buildScc(
  bundle: RepoBundle,
  moduleNode: RepoModule,
  topologyRow: TopologyRow | undefined,
  assertRevision: (moduleNode: RepoModule) => void,
): CouplingSccEvidence {
  const analysis = bundle.aggregates.dependency_topology;
  if (analysis.meta.status === "unavailable") {
    return {
      state: "unknown",
      items: [],
      boundary: {
        status: "unavailable",
        shown: 0,
        declared: null,
        bound: COUPLING_CYCLE_LIMIT,
        reason: analysis.meta.unavailable_reason ??
          "Dependency topology was not produced.",
      },
    };
  }
  const moduleStatus = pageStatus(analysis.modules);
  const cycleStatus = pageStatus(analysis.cycles);
  const cycleIds = topologyRow?.cycle_ids ?? [];
  const cycleById = new Map(analysis.cycles.items.map((cycle) => [
    cycle.id,
    cycle,
  ]));
  const resolved: { row: CycleRow; modules: RepoModule[] }[] = [];
  const moduleById = new Map(bundle.graph.modules.items.map((candidate) => [
    candidate.id,
    candidate,
  ]));
  for (const cycleId of cycleIds) {
    const row = cycleById.get(cycleId);
    if (!row) continue;
    const modules = row.module_ids.map((moduleId) => {
      const referenced = moduleById.get(moduleId);
      if (!referenced) {
        throw new BuildFailure(
          "referenced_module_missing",
          `SCC ${cycleId} references a module outside the captured module page (${moduleId}).`,
        );
      }
      assertRevision(referenced);
      return referenced;
    });
    resolved.push({ row, modules });
  }
  const missingCycleCount = cycleIds.length - resolved.length;
  const sourceReason = combineReasons(
    pageReason(analysis.modules),
    pageReason(analysis.cycles),
    missingCycleCount > 0
      ? `${missingCycleCount} declared SCC record${missingCycleCount === 1 ? " is" : "s are"} outside the captured cycle page.`
      : null,
  );
  const sourceStatus = weakestStatus([
    moduleStatus,
    cycleStatus,
    missingCycleCount > 0 ? "truncated" : "complete",
  ]);
  const items = resolved
    .sort((left, right) => compareText(left.row.id, right.row.id))
    .slice(0, COUPLING_CYCLE_LIMIT)
    .map(({ row, modules }) => {
      const shownModules = modules.slice(0, COUPLING_SCC_MEMBER_LIMIT);
      return {
        id: row.id,
        modules: shownModules,
        memberBoundary: localBoundary({
          sourceStatuses: ["complete"],
          sourceReasons: [],
          shown: shownModules.length,
          captured: modules.length,
          declared: modules.length,
          bound: COUPLING_SCC_MEMBER_LIMIT,
          localReason: `${Math.max(0, modules.length - shownModules.length)} additional captured SCC members are omitted by the fixed display bound.`,
        }),
      };
    });
  const declared = topologyRow ? cycleIds.length : sourceStatus === "complete"
    ? 0
    : null;
  const state = cycleIds.length > 0
    ? "present"
    : topologyRow && sourceStatus === "complete"
    ? "absent"
    : "unknown";
  return {
    state,
    items,
    boundary: localBoundary({
      sourceStatuses: [sourceStatus],
      sourceReasons: [sourceReason],
      shown: items.length,
      captured: resolved.length,
      declared,
      bound: COUPLING_CYCLE_LIMIT,
      localReason: `${Math.max(0, resolved.length - items.length)} additional captured SCC records are omitted by the fixed display bound.`,
    }),
  };
}

function buildHotspot(
  bundle: RepoBundle,
  moduleId: string,
): CouplingHotspotEvidence {
  const analysis = bundle.aggregates.hotspot_quadrant;
  if (analysis.meta.status === "unavailable") {
    return {
      state: "unknown",
      commitCount: null,
      fanIn: null,
      quadrant: null,
      rank: null,
      window: analysis.meta.window,
      boundary: {
        status: "unavailable",
        shown: 0,
        declared: null,
        bound: 1,
        reason: analysis.meta.unavailable_reason ??
          "Hotspot analysis was not produced.",
      },
    };
  }
  const sourceStatus = pageStatus(analysis.data);
  const row = singleRow(
    analysis.data.items.filter((candidate) =>
      candidate.module_id === moduleId
    ),
    `Multiple hotspot rows resolve to ${moduleId}.`,
  );
  const rank = row
    ? analysis.data.items.findIndex((candidate) => candidate === row) + 1
    : null;
  return {
    state: row ? "present" : sourceStatus === "complete" ? "absent" : "unknown",
    commitCount: row?.commit_count ?? null,
    fanIn: row?.fan_in ?? null,
    quadrant: row?.quadrant ?? null,
    rank,
    window: analysis.meta.window,
    boundary: localBoundary({
      sourceStatuses: [sourceStatus],
      sourceReasons: [pageReason(analysis.data)],
      shown: row ? 1 : 0,
      captured: row ? 1 : 0,
      declared: sourceStatus === "complete" ? row ? 1 : 0 : null,
      bound: 1,
    }),
  };
}

function findHistoryRow(
  bundle: RepoBundle,
  moduleId: string,
): HistoryRow | undefined {
  return singleRow(
    bundle.graph.history_navigation.by_module.items.filter((candidate) =>
      candidate.module_id === moduleId
    ),
    `Multiple history-navigation rows resolve to ${moduleId}.`,
  );
}

function buildHistory(
  bundle: RepoBundle,
  row: HistoryRow | undefined,
): CouplingHistoryEvidence {
  const outer = bundle.graph.history_navigation.by_module;
  const outerStatus = pageStatus(outer);
  if (!row) {
    return {
      state: outerStatus === "complete" ? "absent" : "unknown",
      boundary: {
        status: outerStatus,
        shown: 0,
        declared: outerStatus === "complete" ? 0 : null,
        bound: 0,
        reason: pageReason(outer),
      },
    };
  }
  const boundary = pageBoundary(row.commits);
  return {
    state: evidenceState(row.commits.items.length, boundary.status),
    boundary: {
      ...boundary,
      status: weakestStatus([outerStatus, boundary.status]),
      reason: combineReasons(pageReason(outer), boundary.reason),
    },
  };
}

function buildOwnership(
  bundle: RepoBundle,
  moduleId: string,
): CouplingOwnershipEvidence {
  const analysis = bundle.aggregates.ownership;
  const capability = bundle.capability.views.ownership;
  if (analysis.meta.status === "unavailable" || capability.status === "unavailable") {
    const reason = analysis.meta.unavailable_reason ??
      capability.unavailable_reason ??
      "Ownership analysis was not produced.";
    return {
      state: "unknown",
      commitCount: null,
      authorConcentration: null,
      busFactor: null,
      authors: [],
      window: analysis.meta.window,
      boundary: {
        status: "unavailable",
        shown: 0,
        declared: null,
        bound: COUPLING_OWNERSHIP_AUTHOR_LIMIT,
        reason,
      },
      caveat: CAPTURED_OWNERSHIP_CAVEAT,
    };
  }
  const moduleStatus = pageStatus(analysis.modules);
  const row = singleRow(
    analysis.modules.items.filter((candidate) =>
      candidate.module_id === moduleId
    ),
    `Multiple ownership rows resolve to ${moduleId}.`,
  );
  if (!row) {
    return {
      state: moduleStatus === "complete" ? "absent" : "unknown",
      commitCount: null,
      authorConcentration: null,
      busFactor: null,
      authors: [],
      window: analysis.meta.window,
      boundary: {
        status: moduleStatus,
        shown: 0,
        declared: moduleStatus === "complete" ? 0 : null,
        bound: COUPLING_OWNERSHIP_AUTHOR_LIMIT,
        reason: pageReason(analysis.modules),
      },
      caveat: CAPTURED_OWNERSHIP_CAVEAT,
    };
  }
  const authorStatus = pageStatus(row.authors);
  const declaredAuthors = row.authors.total_count.status === "available"
    ? row.authors.total_count.value
    : null;
  const authors = row.authors.items.slice(0, COUPLING_OWNERSHIP_AUTHOR_LIMIT);
  return {
    state: "present",
    commitCount: row.commit_count,
    authorConcentration: row.author_concentration.status === "available"
      ? row.author_concentration.value
      : null,
    busFactor: row.bus_factor.status === "available"
      ? row.bus_factor.value
      : null,
    authors,
    window: analysis.meta.window,
    boundary: localBoundary({
      sourceStatuses: [moduleStatus, authorStatus],
      sourceReasons: [
        pageReason(analysis.modules),
        pageReason(row.authors),
      ],
      shown: authors.length,
      captured: row.authors.items.length,
      declared: declaredAuthors,
      bound: COUPLING_OWNERSHIP_AUTHOR_LIMIT,
      localReason: `${Math.max(0, row.authors.items.length - authors.length)} additional captured author rows are omitted by the fixed display bound.`,
    }),
    caveat: CAPTURED_OWNERSHIP_CAVEAT,
  };
}

function buildSharedCommits(
  bundle: RepoBundle,
  rows: readonly [HistoryRow | undefined, HistoryRow | undefined],
  window: CouplingAnalysisWindow,
  producerCochangeCount: number,
): CouplingSharedCommitEvidence {
  const outerStatus = pageStatus(bundle.graph.history_navigation.by_module);
  const rowStatuses = rows.map((row) => row
    ? pageStatus(row.commits)
    : outerStatus === "complete" ? "complete" : outerStatus
  );
  const rightIds = new Set(rows[1]?.commits.items ?? []);
  const sharedIds = [...new Set(rows[0]?.commits.items ?? [])]
    .filter((commitId) => rightIds.has(commitId));
  const commitPageStatus = pageStatus(bundle.graph.commits);
  const commitById = new Map(bundle.graph.commits.items.map((commit) => [
    commit.id,
    commit,
  ]));
  const resolvedCommits = sharedIds.flatMap((id) => {
    const commit = commitById.get(id);
    return commit ? [commit] : [];
  });
  const missingCommitCount = sharedIds.length - resolvedCommits.length;

  let windowStatus: CouplingCoverageStatus = "complete";
  let windowReason: string | null = null;
  let inWindowCommits = resolvedCommits;
  if (window.kind !== "all_history") {
    const start = window.start ? Date.parse(window.start) : Number.NaN;
    const end = window.end ? Date.parse(window.end) : Number.NaN;
    if (!Number.isFinite(start) || !Number.isFinite(end) || start > end) {
      windowStatus = "truncated";
      windowReason =
        "The captured hidden-coupling window does not declare a valid start and end, so shared commit timestamps cannot be classified.";
      inWindowCommits = [];
    } else {
      const invalidTimestampCount = resolvedCommits.filter((commit) =>
        !Number.isFinite(Date.parse(commit.committed_at))
      ).length;
      if (invalidTimestampCount > 0) {
        windowStatus = "truncated";
        windowReason = `${invalidTimestampCount} shared commit record${invalidTimestampCount === 1 ? " has" : "s have"} no classifiable timestamp for the captured hidden-coupling window.`;
      }
      inWindowCommits = resolvedCommits.filter((commit) => {
        const timestamp = Date.parse(commit.committed_at);
        return Number.isFinite(timestamp) && timestamp >= start && timestamp <= end;
      });
    }
  }

  const evidenceSourceStatuses: CouplingCoverageStatus[] = [
    outerStatus,
    ...rowStatuses,
    commitPageStatus,
    windowStatus,
    missingCommitCount > 0 ? "truncated" : "complete",
  ];
  const evidenceSourceStatus = weakestStatus(evidenceSourceStatuses);
  const exceedsProducerCount = inWindowCommits.length > producerCochangeCount;
  const contradictsProducerCount = exceedsProducerCount ||
    (evidenceSourceStatus === "complete" &&
      inWindowCommits.length !== producerCochangeCount);
  const sourceStatuses: CouplingCoverageStatus[] = [
    ...evidenceSourceStatuses,
    contradictsProducerCount ? "truncated" : "complete",
  ];
  const sourceStatus = weakestStatus(sourceStatuses);
  const sourceReason = combineReasons(
    pageReason(bundle.graph.history_navigation.by_module),
    ...rows.map((row) => row ? pageReason(row.commits) : null),
    rows.some((row) => !row)
      ? "A focused endpoint has no captured history-navigation row."
      : null,
    pageReason(bundle.graph.commits),
    windowReason,
    missingCommitCount > 0
      ? `${missingCommitCount} shared commit ID${missingCommitCount === 1 ? " has" : "s have"} no captured commit record.`
      : null,
    exceedsProducerCount
      ? `${inWindowCommits.length} captured in-window intersections exceed ${producerCochangeCount} producer-declared co-changes.`
      : contradictsProducerCount
      ? `${inWindowCommits.length} captured in-window intersections do not equal ${producerCochangeCount} producer-declared co-changes under complete source coverage.`
      : null,
  );
  const consistentCommits = contradictsProducerCount ? [] : inWindowCommits;
  const items = consistentCommits
    .map((commit) => ({ id: commit.id, commit }))
    .sort((left, right) =>
      compareText(
        right.commit.committed_at,
        left.commit.committed_at,
      ) ||
      compareText(left.commit.sha, right.commit.sha)
    )
    .slice(0, COUPLING_SHARED_COMMIT_LIMIT);
  const declared = sourceStatus === "complete" ? consistentCommits.length : null;
  const state = contradictsProducerCount
    ? "unknown"
    : consistentCommits.length > 0
    ? "present"
    : sourceStatus === "complete" ? "absent" : "unknown";
  return {
    state,
    items,
    boundary: localBoundary({
      sourceStatuses: [sourceStatus],
      sourceReasons: [sourceReason],
      shown: items.length,
      captured: consistentCommits.length,
      declared,
      bound: COUPLING_SHARED_COMMIT_LIMIT,
      localReason: `${Math.max(0, consistentCommits.length - items.length)} additional shared captured commits are omitted by the fixed display bound.`,
    }),
  };
}

type EndpointConnection = Readonly<{
  neighborId: string;
  relation: string;
  direction: CouplingNeighborDirection;
}>;

type EndpointConnectionIndex = Map<
  string,
  Map<string, EndpointConnection>
>;

function addEndpointConnection(
  index: EndpointConnectionIndex,
  connection: EndpointConnection,
): void {
  const connections = index.get(connection.neighborId) ?? new Map();
  connections.set(
    JSON.stringify([connection.relation, connection.direction]),
    connection,
  );
  index.set(connection.neighborId, connections);
}

function endpointConnectionIndexes(
  bundle: RepoBundle,
  endpoints: readonly [RepoModule, RepoModule],
): readonly [EndpointConnectionIndex, EndpointConnectionIndex] {
  const indexes: [EndpointConnectionIndex, EndpointConnectionIndex] = [
    new Map(),
    new Map(),
  ];
  for (const edge of bundle.graph.structure_edges.items) {
    for (const endpointIndex of [0, 1] as const) {
      const endpointId = endpoints[endpointIndex].id;
      const otherEndpointId = endpoints[endpointIndex === 0 ? 1 : 0].id;
      let neighborId: string | null = null;
      let direction: CouplingNeighborDirection | null = null;
      if (edge.source === endpointId && edge.target !== otherEndpointId) {
        neighborId = edge.target;
        direction = "outgoing";
      } else if (
        edge.target === endpointId && edge.source !== otherEndpointId
      ) {
        neighborId = edge.source;
        direction = "incoming";
      }
      if (neighborId && direction) {
        addEndpointConnection(indexes[endpointIndex], {
          neighborId,
          relation: String(edge.relation),
          direction,
        });
      }
    }
  }
  return indexes;
}

function sortedEndpointConnections(
  index: EndpointConnectionIndex,
  neighborId: string,
): EndpointConnection[] {
  return [...(index.get(neighborId)?.values() ?? [])].sort(
    (left, right) =>
      compareText(left.relation, right.relation) ||
      compareText(left.direction, right.direction),
  );
}

function buildCommonNeighbors(
  bundle: RepoBundle,
  endpoints: readonly [RepoModule, RepoModule],
  assertRevision: (moduleNode: RepoModule) => void,
): CouplingCommonNeighborEvidence {
  const structurePage = bundle.graph.structure_edges;
  const modulePage = bundle.graph.modules;
  const structureStatus = pageStatus(structurePage);
  const moduleStatus = pageStatus(modulePage);
  const moduleById = new Map(bundle.graph.modules.items.map((moduleNode) => [
    moduleNode.id,
    moduleNode,
  ]));
  const knownNonModuleIds = new Set([
    bundle.graph.repository.id,
    ...bundle.graph.packages.items.map((item) => item.id),
    ...bundle.graph.functions.items.map((item) => item.id),
    ...bundle.graph.datatypes.items.map((item) => item.id),
    ...bundle.graph.interfaces.items.map((item) => item.id),
    ...bundle.graph.commits.items.map((item) => item.id),
    ...bundle.graph.issues.items.map((item) => item.id),
    ...bundle.graph.pull_requests.items.map((item) => item.id),
  ]);
  const [leftIndex, rightIndex] = endpointConnectionIndexes(bundle, endpoints);
  const unresolvedNeighborIds = new Set<string>();
  const groups: Array<Readonly<{
    module: RepoModule;
    left: readonly EndpointConnection[];
    right: readonly EndpointConnection[];
  }>> = [];
  const commonNeighborIds = [...leftIndex.keys()]
    .filter((neighborId) => rightIndex.has(neighborId))
    .sort(compareText);
  for (const neighborId of commonNeighborIds) {
    const moduleNode = moduleById.get(neighborId);
    if (!moduleNode) {
      if (!knownNonModuleIds.has(neighborId)) {
        unresolvedNeighborIds.add(neighborId);
      }
      continue;
    }
    assertRevision(moduleNode);
    groups.push({
      module: moduleNode,
      left: sortedEndpointConnections(leftIndex, neighborId),
      right: sortedEndpointConnections(rightIndex, neighborId),
    });
  }
  if (unresolvedNeighborIds.size > 0 && moduleStatus === "complete") {
    throw new BuildFailure(
      "referenced_module_missing",
      `Captured common-neighbor evidence references a module outside the complete captured module page (${[...unresolvedNeighborIds].sort(compareText).join(", ")}).`,
    );
  }
  const unresolvedReason = unresolvedNeighborIds.size > 0
    ? `${unresolvedNeighborIds.size} captured common-neighbor ID${unresolvedNeighborIds.size === 1 ? " is" : "s are"} outside the captured module page.`
    : null;
  const sourceStatus = weakestStatus([
    structureStatus,
    moduleStatus,
    unresolvedNeighborIds.size > 0 ? "truncated" : "complete",
  ]);
  groups.sort((left, right) =>
    compareText(left.module.source_path, right.module.source_path) ||
    compareText(left.module.id, right.module.id)
  );
  let capturedCount = 0;
  const items: CouplingCommonNeighbor[] = [];
  for (const group of groups) {
    capturedCount += group.left.length * group.right.length;
    if (items.length >= COUPLING_COMMON_NEIGHBOR_LIMIT) continue;
    for (const leftConnection of group.left) {
      for (const rightConnection of group.right) {
        items.push({
          module: group.module,
          leftDirection: leftConnection.direction,
          leftRelation: leftConnection.relation,
          rightDirection: rightConnection.direction,
          rightRelation: rightConnection.relation,
        });
        if (items.length >= COUPLING_COMMON_NEIGHBOR_LIMIT) break;
      }
      if (items.length >= COUPLING_COMMON_NEIGHBOR_LIMIT) break;
    }
  }
  const declared = sourceStatus === "complete" ? capturedCount : null;
  return {
    state: evidenceState(capturedCount, sourceStatus),
    items,
    boundary: localBoundary({
      sourceStatuses: [sourceStatus],
      sourceReasons: [combineReasons(
        pageReason(structurePage),
        pageReason(modulePage),
        unresolvedReason,
      )],
      shown: items.length,
      captured: capturedCount,
      declared,
      bound: COUPLING_COMMON_NEIGHBOR_LIMIT,
      localReason: `${Math.max(0, capturedCount - items.length)} additional captured common neighbors are omitted by the fixed display bound.`,
    }),
  };
}

function buildDirectDependency(
  bundle: RepoBundle,
  endpoints: readonly [RepoModule, RepoModule],
): CouplingDirectDependencyEvidence {
  const structurePage = bundle.graph.structure_edges;
  const sourceStatus = pageStatus(structurePage);
  const directions = new Set<CouplingDependencyDirection>();
  for (const edge of structurePage.items) {
    if (edge.relation !== "depends_on") continue;
    if (edge.source === endpoints[0].id && edge.target === endpoints[1].id) {
      directions.add("left_to_right");
    }
    if (edge.source === endpoints[1].id && edge.target === endpoints[0].id) {
      directions.add("right_to_left");
    }
  }
  const ordered = [...directions].sort(compareText);
  const declared = sourceStatus === "complete" ? ordered.length : null;
  return {
    state: evidenceState(ordered.length, sourceStatus),
    directions: ordered,
    boundary: localBoundary({
      sourceStatuses: [sourceStatus],
      sourceReasons: [pageReason(structurePage)],
      shown: ordered.length,
      captured: ordered.length,
      declared,
      bound: COUPLING_DIRECT_DEPENDENCY_LIMIT,
    }),
  };
}

function verifyPrompts(comparison: Readonly<{
  endpoints: readonly [CouplingEndpointEvidence, CouplingEndpointEvidence];
  commonNeighbors: CouplingCommonNeighborEvidence;
  directDependency: CouplingDirectDependencyEvidence;
  sharedCommits: CouplingSharedCommitEvidence;
}>): string[] {
  const [left, right] = comparison.endpoints;
  const prompts = [
    `Inspect ${left.module.source_path} and ${right.module.source_path} at the recorded SHA; compare the captured changes before confirming or refuting the boundary candidate.`,
  ];
  if (comparison.sharedCommits.state === "present") {
    prompts.push(
      "Read the bounded shared-commit sample and inspect the changes that touched both endpoints.",
    );
  }
  if (comparison.commonNeighbors.state === "present") {
    prompts.push(
      `Inspect whether the captured shared neighbors (${comparison.commonNeighbors.items.map((item) => item.module.source_path).join(", ")}) explain the repeated co-change.`,
    );
  }
  if (comparison.directDependency.state === "absent") {
    prompts.push(
      "Verify in source whether the complete captured structure page's direct-dependency absence matches the endpoint imports.",
    );
  } else if (comparison.directDependency.state === "unknown") {
    prompts.push(
      "Inspect the endpoint imports because the captured structure coverage cannot decide direct-dependency absence.",
    );
  }
  if (comparison.endpoints.some((endpoint) =>
    endpoint.ownership.state === "present"
  )) {
    prompts.push(
      "Compare captured author rows with review history before drawing an ownership conclusion.",
    );
  }
  return prompts;
}

export function buildCouplingComparison({
  bundle,
  sourcePaths,
}: Readonly<{
  bundle: RepoBundle;
  sourcePaths: readonly [string, string];
}>): CouplingComparisonResult {
  try {
    const canonicalPaths = [...sourcePaths].sort(compareText) as [string, string];
    if (canonicalPaths[0] === canonicalPaths[1]) {
      throw new BuildFailure(
        "pair_unresolved",
        "A coupling comparison requires two distinct source paths.",
      );
    }
    const endpoints = canonicalPaths.map((sourcePath) => {
      const matches = bundle.graph.modules.items.filter((moduleNode) =>
        moduleNode.source_path === sourcePath
      );
      if (matches.length !== 1) {
        throw new BuildFailure(
          "pair_unresolved",
          `Focused source path does not resolve to one captured module (${sourcePath}).`,
        );
      }
      return matches[0];
    }) as [RepoModule, RepoModule];
    const revision = bundle.meta.snapshot.head_sha;
    const assertRevision = (moduleNode: RepoModule) =>
      assertModuleRevision(moduleNode, revision);
    endpoints.forEach(assertRevision);

    const pairAnalysis = bundle.aggregates.hidden_coupling;
    if (
      pairAnalysis.meta.status === "unavailable" ||
      pairAnalysis.data.disclosure.status === "unavailable"
    ) {
      throw new BuildFailure(
        "pair_evidence_unavailable",
        pairAnalysis.meta.unavailable_reason ??
          pairAnalysis.data.disclosure.reason ??
          "Hidden-coupling evidence is unavailable.",
      );
    }
    const pairKey = structureCouplingPairKey(endpoints[0].id, endpoints[1].id);
    const pairRows = pairAnalysis.data.items.filter((row) =>
      structureCouplingPairKey(row.left_module_id, row.right_module_id) ===
        pairKey
    );
    if (pairRows.length === 0) {
      throw new BuildFailure(
        "pair_evidence_unavailable",
        "The focused paths do not resolve to one captured coupling row.",
      );
    }
    const pairRow = singleRow(
      pairRows,
      "Multiple hidden-coupling rows resolve to the focused pair.",
    )!;

    for (const edge of bundle.graph.structure_edges.items) {
      if (
        edge.source === endpoints[0].id ||
        edge.target === endpoints[0].id ||
        edge.source === endpoints[1].id ||
        edge.target === endpoints[1].id
      ) assertEdgeRevision(edge, revision);
    }

    const endpointRows = endpoints.map((moduleNode) => {
      const topology = buildTopology(bundle, moduleNode.id);
      const historyRow = findHistoryRow(bundle, moduleNode.id);
      return {
        module: moduleNode,
        historyRow,
        evidence: {
          module: moduleNode,
          topology: topology.evidence,
          scc: buildScc(
            bundle,
            moduleNode,
            topology.row,
            assertRevision,
          ),
          hotspot: buildHotspot(bundle, moduleNode.id),
          history: buildHistory(bundle, historyRow),
          ownership: buildOwnership(bundle, moduleNode.id),
        } satisfies CouplingEndpointEvidence,
      };
    }) as [
      { module: RepoModule; historyRow: HistoryRow | undefined; evidence: CouplingEndpointEvidence },
      { module: RepoModule; historyRow: HistoryRow | undefined; evidence: CouplingEndpointEvidence },
    ];
    const endpointEvidence = endpointRows.map((entry) => entry.evidence) as [
      CouplingEndpointEvidence,
      CouplingEndpointEvidence,
    ];
    const sharedCommits = buildSharedCommits(bundle, [
      endpointRows[0].historyRow,
      endpointRows[1].historyRow,
    ], pairAnalysis.meta.window, pairRow.cochange_count);
    const commonNeighbors = buildCommonNeighbors(
      bundle,
      endpoints,
      assertRevision,
    );
    const directDependency = buildDirectDependency(bundle, endpoints);
    const partial = {
      endpoints: endpointEvidence,
      sharedCommits,
      commonNeighbors,
      directDependency,
    };
    const value: CouplingComparison = {
      sourceRevision: revision,
      endpoints: endpointEvidence,
      cochange: {
        state: "present",
        count: pairRow.cochange_count,
        support: pairRow.support,
        boundary: pageBoundary(pairAnalysis.data),
      },
      sharedCommits,
      commonNeighbors,
      directDependency,
      caveat:
        "This is a coupling candidate, not a defect. Captured evidence narrows source inspection; it does not decide the architectural interpretation.",
      verifyPrompts: verifyPrompts(partial),
    };
    return { status: "available", value };
  } catch (error) {
    if (error instanceof BuildFailure) {
      return { status: "unavailable", code: error.code, reason: error.message };
    }
    throw error;
  }
}
