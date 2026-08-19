import type {
  RepoBundle,
  RepoCommit,
  RepoModule,
  ViewId,
} from "@/lib/repo-bundle";

type HotspotRow =
  RepoBundle["aggregates"]["hotspot_quadrant"]["data"]["items"][number];
type OwnershipRow =
  RepoBundle["aggregates"]["ownership"]["modules"]["items"][number];
type AnalysisWindow =
  RepoBundle["aggregates"]["hotspot_quadrant"]["meta"]["window"];

export type RepositorySignalKind =
  | "hotspot"
  | "dependency_cycle"
  | "hidden_coupling"
  | "ownership";

export type RepositorySignalClassification = "observed" | "candidate";

export interface RepositoryEvidence {
  label: string;
  value: string;
  detail?: string;
}

export interface RepositoryAttentionSignal {
  id: string;
  kind: RepositorySignalKind;
  classification: RepositorySignalClassification;
  title: string;
  summary: string;
  whyItMatters: string;
  moduleIds: string[];
  evidence: RepositoryEvidence[];
  targetView: ViewId;
}

export interface RepositoryStartHereEntry {
  moduleId: string;
  moduleName: string;
  modulePath: string;
  sourcePath: string;
  dependentCount: number;
  commitCount: number | null;
  reason: string;
  evidence: RepositoryEvidence[];
}

export interface RepositoryMetric {
  shown: number;
  total: number | null;
  bound: number | null;
  status: "complete" | "truncated" | "unavailable";
  reason: string | null;
  summary: string;
  detail: string;
}

export interface RepositoryBrief {
  repository: {
    name: string;
    owner: string;
    canonicalUrl: string;
    headSha: string;
    ingestedAt: string;
  };
  metrics: {
    packages: RepositoryMetric;
    modules: RepositoryMetric;
    commits: RepositoryMetric;
    cycles: RepositoryMetric;
  };
  startHere: RepositoryStartHereEntry[];
  startHereState: RepositoryMetric;
  attentionSignals: RepositoryAttentionSignal[];
  attentionState: RepositoryMetric;
  evidence: RepositoryEvidence[];
}

export type EvidenceItem = RepositoryEvidence;
export type SignalClassification = RepositorySignalClassification;
export type RepositorySignal = RepositoryAttentionSignal;
export type StartHereEntry = RepositoryStartHereEntry;

export interface ModuleCycle {
  id: string;
  moduleIds: string[];
  modules: RepoModule[];
}

export interface ModuleCoupling {
  module: RepoModule;
  cochangeCount: number;
  support: number;
}

export interface ModuleInsight {
  module: RepoModule;
  topology: {
    fanIn: number;
    fanOut: number;
    coverage: RepositoryMetric;
    cycleIds: string[];
    cycles: ModuleCycle[];
  };
  hotspot: {
    commitCount: number;
    fanIn: number;
    quadrant: HotspotRow["quadrant"];
  } | null;
  ownership: {
    commitCount: number;
    authorConcentration: number | null;
    busFactor: number | null;
    authors: OwnershipRow["authors"]["items"];
  } | null;
  apiSurface: {
    dependentCount: number;
    rank: number;
  } | null;
  dependencies: RepoModule[];
  dependents: RepoModule[];
  history: RepositoryMetric;
  recentCommits: RepoCommit[];
  couplings: ModuleCoupling[];
  couplingState: RepositoryMetric;
  evidence: RepositoryEvidence[];
}

const RECENT_COMMIT_LIMIT = 12;
const OWNERSHIP_MINIMUM_COMMITS = 5;

function compareText(left: string, right: string): number {
  if (left < right) return -1;
  if (left > right) return 1;
  return 0;
}

function modulePath(
  moduleById: ReadonlyMap<string, RepoModule>,
  moduleId: string,
): string {
  return moduleById.get(moduleId)?.source_path ?? moduleId;
}

function compareModules(left: RepoModule, right: RepoModule): number {
  return (
    compareText(left.source_path, right.source_path) ||
    compareText(left.id, right.id)
  );
}

function uniqueSortedModules(modules: RepoModule[]): RepoModule[] {
  return [
    ...new Map(modules.map((module) => [module.id, module])).values(),
  ].sort(compareModules);
}

function formatWindow(window: AnalysisWindow, snapshotTime: string): string {
  if (window.kind === "rolling_days") {
    const duration = window.days == null ? "Rolling" : `${window.days}-day`;
    const range = window.start && window.end
      ? ` (${window.start} to ${window.end})`
      : "";
    return `${duration} analysis window${range}`;
  }
  if (window.kind === "range") {
    return `Bounded range ${window.start ?? "unspecified start"} to ${
      window.end ?? "unspecified end"
    }`;
  }
  return `Declared all-history window; snapshot ingested at ${snapshotTime}`;
}

function pageEvidence(
  label: string,
  page: {
    items: unknown[];
    total_count:
      | { status: "available"; value: number }
      | { status: "unavailable"; reason: string };
    bound: { kind: "all" | "top_n"; max_items: number; order: string };
    truncated: boolean;
    next_cursor?: string | null;
    disclosure: {
      status: "complete" | "truncated" | "unavailable";
      reason?: string | null;
    };
  },
  labels: RepoBundle["capability"]["labels"],
): RepositoryEvidence {
  const total = page.total_count.status === "available"
    ? `${page.total_count.value} declared`
    : `total ${labels.unavailable}`;
  const reasonSuffix = page.disclosure.reason
    ? `: ${page.disclosure.reason}`
    : "";
  const status = page.disclosure.status === "unavailable"
    ? `${labels.unavailable}${reasonSuffix}`
    : page.disclosure.status === "truncated" || page.next_cursor != null
    ? `${labels.truncated}${reasonSuffix}`
    : "complete";
  return {
    label,
    value: `${page.items.length} present, ${total}; ${status}`,
    detail:
      `Bound ${page.bound.kind} to ${page.bound.max_items}, ordered by ${page.bound.order}.`,
  };
}

function sourceRoleEvidence(): RepositoryEvidence {
  return {
    label: "Source-role scope",
    value: "Not classified in this snapshot",
    detail:
      "Captured module rows can include production, test, example, and generated sources; verify the path before treating a rank as production architecture.",
  };
}

function pageMetric(
  page: {
  items: unknown[];
  total_count:
    | { status: "available"; value: number }
    | { status: "unavailable"; reason: string };
  bound: { kind: "all" | "top_n"; max_items: number; order: string };
  next_cursor?: string | null;
  truncated: boolean;
  disclosure: {
    status: "complete" | "truncated" | "unavailable";
    reason?: string | null;
  };
  },
  labels: RepoBundle["capability"]["labels"],
): RepositoryMetric {
  const shown = page.items.length;
  const total = page.total_count.status === "available"
    ? page.total_count.value
    : null;
  const status: RepositoryMetric["status"] =
    page.disclosure.status === "unavailable"
      ? "unavailable"
      : page.truncated ||
          page.next_cursor != null ||
          page.disclosure.status === "truncated" ||
          (total != null && total > shown)
      ? "truncated"
      : "complete";
  const reason = page.disclosure.reason ??
    (status !== "complete" && page.total_count.status === "unavailable"
      ? page.total_count.reason
      : null);
  const reasonSuffix = reason ? `; ${reason}` : "";
  const summary = status === "complete"
    ? total == null
      ? `${shown} captured; total ${labels.unavailable}`
      : `${shown} captured; complete`
    : status === "truncated"
    ? total == null
      ? `${shown} captured; total ${labels.unavailable}; ${labels.truncated}${reasonSuffix}`
      : `${shown} captured of ${total}; ${labels.truncated}${reasonSuffix}`
    : `${shown} captured; total ${labels.unavailable}; ${labels.unavailable}${reasonSuffix}`;
  return {
    shown,
    total,
    bound: page.bound.max_items,
    status,
    reason,
    summary,
    detail:
      `Bound ${page.bound.kind} to ${page.bound.max_items}, ordered by ${page.bound.order}.`,
  };
}

function unavailableMetric(
  labels: RepoBundle["capability"]["labels"],
  reason: string,
): RepositoryMetric {
  return {
    shown: 0,
    total: null,
    bound: null,
    status: "unavailable",
    reason,
    summary: `${labels.unavailable}: ${reason}`,
    detail: reason,
  };
}

function missingHistoryNavigationMetric(
  coverage: {
    bound: { kind: "all" | "top_n"; max_items: number; order: string };
    next_cursor?: string | null;
    disclosure: {
      status: "complete" | "truncated" | "unavailable";
      reason?: string | null;
    };
  },
  labels: RepoBundle["capability"]["labels"],
): RepositoryMetric {
  if (coverage.disclosure.status === "unavailable") {
    return unavailableMetric(
      labels,
      coverage.disclosure.reason ?? "History navigation was not produced.",
    );
  }
  if (
    coverage.disclosure.status === "truncated" ||
    coverage.next_cursor != null
  ) {
    const reason = coverage.disclosure.reason ??
      "The by-module history-navigation page was truncated before reaching this module.";
    return {
      shown: 0,
      total: null,
      bound: coverage.bound.max_items,
      status: "truncated",
      reason,
      summary: `0 captured; ${labels.truncated}: ${reason}`,
      detail:
        `Bound ${coverage.bound.kind} to ${coverage.bound.max_items}, ordered by ${coverage.bound.order}; this module's row may exist beyond that bound.`,
    };
  }
  return unavailableMetric(
    labels,
    "No module history-navigation row was captured.",
  );
}

function analysisMetric(
  analysis: {
    meta: {
      status: "available" | "unavailable";
      unavailable_reason?: string | null;
    };
    data: Parameters<typeof pageMetric>[0];
  },
  labels: RepoBundle["capability"]["labels"],
): RepositoryMetric {
  if (analysis.meta.status === "unavailable") {
    return unavailableMetric(
      labels,
      analysis.meta.unavailable_reason ?? "The analysis was not produced.",
    );
  }
  return pageMetric(analysis.data, labels);
}

function combinedAttentionMetric(
  metrics: readonly RepositoryMetric[],
  shown: number,
  labels: RepoBundle["capability"]["labels"],
): RepositoryMetric {
  const unavailable = metrics.filter((metric) =>
    metric.status === "unavailable"
  );
  if (unavailable.length > 0) {
    const reason = unavailable
      .map((metric) => metric.reason)
      .filter(Boolean)
      .join("; ") || "One or more attention analyses were not produced.";
    return unavailableMetric(labels, reason);
  }
  const truncated = metrics.filter((metric) => metric.status === "truncated");
  if (truncated.length > 0) {
    const reason = truncated
      .map((metric) => metric.reason)
      .filter(Boolean)
      .join("; ") || "One or more attention analyses were truncated.";
    return {
      shown,
      total: shown,
      bound: shown,
      status: "truncated",
      reason,
      summary: `${shown} signals from available analyses; ${labels.truncated}: ${reason}`,
      detail:
        "Each signal carries its own capability-owned row coverage and export bound.",
    };
  }
  return {
    shown,
    total: shown,
    bound: shown,
    status: "complete",
    reason: null,
    summary: `${shown} signals from available analyses`,
    detail:
      "Each signal carries its own capability-owned row coverage and export bound.",
  };
}

function hotspotComparator(
  moduleById: ReadonlyMap<string, RepoModule>,
): (left: HotspotRow, right: HotspotRow) => number {
  return (left, right) => {
    const leftPriority = left.quadrant === "high_churn_high_fan_in" ? 0 : 1;
    const rightPriority = right.quadrant === "high_churn_high_fan_in" ? 0 : 1;
    return (
      leftPriority - rightPriority ||
      right.commit_count - left.commit_count ||
      right.fan_in - left.fan_in ||
      compareText(
        modulePath(moduleById, left.module_id),
        modulePath(moduleById, right.module_id),
      ) ||
      compareText(left.module_id, right.module_id)
    );
  };
}

function buildHotspotSignal(
  bundle: RepoBundle,
  moduleById: ReadonlyMap<string, RepoModule>,
): RepositoryAttentionSignal | null {
  const analysis = bundle.aggregates.hotspot_quadrant;
  if (analysis.meta.status !== "available") return null;
  const hotspot = analysis.data.items
    .filter(
      (row) =>
        moduleById.has(row.module_id) &&
        row.quadrant !== "low_churn_low_fan_in",
    )
    .sort(hotspotComparator(moduleById))[0];
  if (!hotspot) return null;

  const moduleNode = moduleById.get(hotspot.module_id)!;
  return {
    id: `hotspot:${moduleNode.id}`,
    kind: "hotspot",
    classification: "candidate",
    title:
      `${moduleNode.source_path} is a ${bundle.capability.views.hotspot_quadrant.label} candidate`,
    summary:
      `${hotspot.commit_count} captured ${bundle.capability.labels.metrics.commits} and ${bundle.capability.labels.metrics.fan_in} ${hotspot.fan_in} place this module in ${
        bundle.capability.labels.hotspot_quadrants[hotspot.quadrant]
      }.`,
    whyItMatters:
      "Frequent change combined with incoming dependencies can increase review scope. These measurements identify where to inspect; they do not establish a defect.",
    moduleIds: [moduleNode.id],
    evidence: [
      {
        label: "Analysis window",
        value: formatWindow(
          analysis.meta.window,
          bundle.meta.snapshot.ingested_at,
        ),
      },
      {
        label: bundle.capability.views.hotspot_quadrant.label,
        value:
          `${hotspot.commit_count} ${bundle.capability.labels.metrics.commits}; ${bundle.capability.labels.metrics.fan_in} ${hotspot.fan_in}`,
      },
      sourceRoleEvidence(),
      pageEvidence("Coverage", analysis.data, bundle.capability.labels),
    ],
    targetView: "hotspot_quadrant",
  };
}

function buildCycleSignal(
  bundle: RepoBundle,
  moduleById: ReadonlyMap<string, RepoModule>,
): RepositoryAttentionSignal | null {
  const analysis = bundle.aggregates.dependency_topology;
  if (analysis.meta.status !== "available") return null;

  const hotspotByModule = new Map(
    bundle.aggregates.hotspot_quadrant.meta.status === "available"
      ? bundle.aggregates.hotspot_quadrant.data.items.map((row) => [
        row.module_id,
        row,
      ] as const)
      : [],
  );
  const topologyByModule = new Map(
    analysis.modules.items.map((row) => [row.module_id, row]),
  );
  const cycle = [...analysis.cycles.items].sort((left, right) => {
    const churn = (item: typeof left) =>
      item.module_ids.reduce(
        (total, moduleId) =>
          total + (hotspotByModule.get(moduleId)?.commit_count ?? 0),
        0,
      );
    const fanIn = (item: typeof left) =>
      item.module_ids.reduce(
        (total, moduleId) =>
          total + (topologyByModule.get(moduleId)?.fan_in ?? 0),
        0,
      );
    return (
      churn(right) - churn(left) ||
      fanIn(right) - fanIn(left) ||
      right.module_ids.length - left.module_ids.length ||
      compareText(left.id, right.id)
    );
  })[0];
  if (!cycle) return null;

  const paths = cycle.module_ids.map((moduleId) =>
    modulePath(moduleById, moduleId)
  );
  return {
    id: `dependency-cycle:${cycle.id}`,
    kind: "dependency_cycle",
    classification: "observed",
    title:
      `Observed ${bundle.capability.views.dependency_topology.label} across ${cycle.module_ids.length} ${bundle.capability.labels.node_types.module.toLocaleLowerCase()} records`,
    summary: `SCC members: ${paths.join(" · ")}`,
    whyItMatters:
      "The captured import graph contains a cycle, which can complicate change sequencing and boundary review. It does not by itself imply a runtime failure.",
    moduleIds: [...cycle.module_ids],
    evidence: [
      {
        label: "Analysis window",
        value: formatWindow(
          analysis.meta.window,
          bundle.meta.snapshot.ingested_at,
        ),
      },
      {
        label: bundle.capability.views.dependency_topology.label,
        value:
          `${cycle.id}; ${cycle.module_ids.length} ${bundle.capability.labels.node_types.module.toLocaleLowerCase()} records`,
      },
      sourceRoleEvidence(),
      pageEvidence("Coverage", analysis.cycles, bundle.capability.labels),
    ],
    targetView: "dependency_topology",
  };
}

function buildCouplingSignal(
  bundle: RepoBundle,
  moduleById: ReadonlyMap<string, RepoModule>,
): RepositoryAttentionSignal | null {
  const analysis = bundle.aggregates.hidden_coupling;
  if (analysis.meta.status !== "available") return null;
  const coupling = analysis.data.items
    .filter(
      (row) =>
        moduleById.has(row.left_module_id) &&
        moduleById.has(row.right_module_id),
    )
    .sort(
      (left, right) =>
        right.cochange_count - left.cochange_count ||
        right.support - left.support ||
        compareText(
          modulePath(moduleById, left.left_module_id),
          modulePath(moduleById, right.left_module_id),
        ) ||
        compareText(
          modulePath(moduleById, left.right_module_id),
          modulePath(moduleById, right.right_module_id),
        ) ||
        compareText(left.left_module_id, right.left_module_id) ||
        compareText(left.right_module_id, right.right_module_id),
    )[0];
  if (!coupling) return null;

  const left = moduleById.get(coupling.left_module_id)!;
  const right = moduleById.get(coupling.right_module_id)!;
  return {
    id: `hidden-coupling:${left.id}:${right.id}`,
    kind: "hidden_coupling",
    classification: "candidate",
    title:
      `${left.name} and ${right.name} are a ${bundle.capability.views.hidden_coupling.label} candidate`,
    summary:
      `${left.source_path} and ${right.source_path} changed together ${coupling.cochange_count} times in the captured window.`,
    whyItMatters:
      "Repeated co-change can reveal coordination cost or an implicit boundary worth inspecting. Co-change alone does not prove a dependency or defect.",
    moduleIds: [left.id, right.id],
    evidence: [
      {
        label: "Analysis window",
        value: formatWindow(
          analysis.meta.window,
          bundle.meta.snapshot.ingested_at,
        ),
      },
      {
        label: bundle.capability.views.hidden_coupling.label,
        value:
          `${coupling.cochange_count} ${bundle.capability.labels.metrics.cochange_count}; ${bundle.capability.labels.metrics.support} ${
            (coupling.support * 100).toFixed(1)
          }%`,
      },
      sourceRoleEvidence(),
      pageEvidence("Coverage", analysis.data, bundle.capability.labels),
    ],
    targetView: "hidden_coupling",
  };
}

function buildOwnershipSignal(
  bundle: RepoBundle,
  moduleById: ReadonlyMap<string, RepoModule>,
): RepositoryAttentionSignal | null {
  const analysis = bundle.aggregates.ownership;
  if (
    analysis.meta.status !== "available" ||
    bundle.capability.views.ownership.status !== "available"
  ) return null;
  const ownership = analysis.modules.items
    .filter(
      (row) =>
        row.commit_count >= OWNERSHIP_MINIMUM_COMMITS &&
        moduleById.has(row.module_id) &&
        row.bus_factor.status === "available" &&
        row.bus_factor.value <= 1,
    )
    .sort((left, right) => {
      const leftConcentration = left.author_concentration.status === "available"
        ? left.author_concentration.value
        : -1;
      const rightConcentration =
        right.author_concentration.status === "available"
          ? right.author_concentration.value
          : -1;
      return (
        rightConcentration - leftConcentration ||
        right.commit_count - left.commit_count ||
        compareText(
          modulePath(moduleById, left.module_id),
          modulePath(moduleById, right.module_id),
        ) ||
        compareText(left.module_id, right.module_id)
      );
    })[0];
  if (!ownership) return null;

  if (ownership.bus_factor.status !== "available") return null;
  const moduleNode = moduleById.get(ownership.module_id)!;
  const concentration = ownership.author_concentration.status === "available"
    ? `${
      (ownership.author_concentration.value * 100).toFixed(1)
    }% ${bundle.capability.labels.metrics.author_concentration}`
    : `${bundle.capability.labels.metrics.author_concentration} ${bundle.capability.labels.unavailable}`;
  return {
    id: `ownership:${moduleNode.id}`,
    kind: "ownership",
    classification: "candidate",
    title:
      `${moduleNode.source_path} has concentrated captured ${bundle.capability.views.ownership.label}`,
    summary:
      `${ownership.commit_count} captured ${bundle.capability.labels.metrics.commits}, ${bundle.capability.labels.metrics.bus_factor} ${ownership.bus_factor.value}, and ${concentration}.`,
    whyItMatters:
      "Concentrated contribution history can indicate a review or knowledge-transfer candidate. It does not establish current team ownership or availability.",
    moduleIds: [moduleNode.id],
    evidence: [
      {
        label: "Analysis window",
        value: formatWindow(
          analysis.meta.window,
          bundle.meta.snapshot.ingested_at,
        ),
      },
      {
        label: "Sample threshold",
        value:
          `${ownership.commit_count} ${bundle.capability.labels.metrics.commits} (minimum ${OWNERSHIP_MINIMUM_COMMITS})`,
      },
      sourceRoleEvidence(),
      pageEvidence("Coverage", analysis.modules, bundle.capability.labels),
    ],
    targetView: "ownership",
  };
}

export function buildRepositoryBrief(bundle: RepoBundle): RepositoryBrief {
  const labels = bundle.capability.labels;
  const moduleById = new Map(
    bundle.graph.modules.items.map((module) => [module.id, module]),
  );
  const hotspotByModule = new Map(
    bundle.aggregates.hotspot_quadrant.meta.status === "available"
      ? bundle.aggregates.hotspot_quadrant.data.items.map((row) => [
        row.module_id,
        row,
      ] as const)
      : [],
  );
  const startHere = bundle.aggregates.api_surface.meta.status === "available"
    ? [...bundle.aggregates.api_surface.data.items]
      .filter(
        (row) => row.dependent_count > 0 && moduleById.has(row.module_id),
      )
      .sort(
        (left, right) =>
          right.dependent_count - left.dependent_count ||
          compareText(
            modulePath(moduleById, left.module_id),
            modulePath(moduleById, right.module_id),
          ) ||
          compareText(left.module_id, right.module_id),
      )
      .slice(0, 3)
      .map((row): RepositoryStartHereEntry => {
        const moduleNode = moduleById.get(row.module_id)!;
        const hotspot = hotspotByModule.get(row.module_id);
        return {
          moduleId: moduleNode.id,
          moduleName: moduleNode.name,
          modulePath: moduleNode.module_path,
          sourcePath: moduleNode.source_path,
          dependentCount: row.dependent_count,
          commitCount: hotspot?.commit_count ?? null,
          reason:
            "Many captured modules depend on this module; inspect its contract before broad changes.",
          evidence: [
            {
              label: bundle.capability.views.api_surface.label,
              value:
                `${row.dependent_count} ${bundle.capability.labels.metrics.dependent_count}`,
            },
            {
              label: "Analysis window",
              value: formatWindow(
                bundle.aggregates.api_surface.meta.window,
                bundle.meta.snapshot.ingested_at,
              ),
            },
            sourceRoleEvidence(),
          ],
        };
      })
    : [];

  const attentionSignals = [
    buildHotspotSignal(bundle, moduleById),
    buildCycleSignal(bundle, moduleById),
    buildCouplingSignal(bundle, moduleById),
    buildOwnershipSignal(bundle, moduleById),
  ].filter((signal): signal is RepositoryAttentionSignal => signal !== null);
  const startHereState = analysisMetric(
    bundle.aggregates.api_surface,
    labels,
  );
  const attentionState = combinedAttentionMetric(
    [
      analysisMetric(bundle.aggregates.hotspot_quadrant, labels),
      analysisMetric(
        {
          meta: bundle.aggregates.dependency_topology.meta,
          data: bundle.aggregates.dependency_topology.cycles,
        },
        labels,
      ),
      analysisMetric(bundle.aggregates.hidden_coupling, labels),
      analysisMetric(
        {
          meta: bundle.aggregates.ownership.meta,
          data: bundle.aggregates.ownership.modules,
        },
        labels,
      ),
    ],
    attentionSignals.length,
    labels,
  );

  return {
    repository: {
      name: bundle.meta.repository.name,
      owner: bundle.meta.repository.owner,
      canonicalUrl: bundle.meta.repository.canonical_url,
      headSha: bundle.meta.snapshot.head_sha,
      ingestedAt: bundle.meta.snapshot.ingested_at,
    },
    metrics: {
      packages: pageMetric(bundle.graph.packages, labels),
      modules: pageMetric(bundle.graph.modules, labels),
      commits: pageMetric(bundle.graph.commits, labels),
      cycles: analysisMetric(
        {
          meta: bundle.aggregates.dependency_topology.meta,
          data: bundle.aggregates.dependency_topology.cycles,
        },
        labels,
      ),
    },
    startHere,
    startHereState,
    attentionSignals,
    attentionState,
    evidence: [
      {
        label: "Snapshot",
        value:
          `${bundle.meta.snapshot.head_sha} ingested at ${bundle.meta.snapshot.ingested_at}`,
      },
      pageEvidence("Module coverage", bundle.graph.modules, labels),
      pageEvidence("Commit coverage", bundle.graph.commits, labels),
      sourceRoleEvidence(),
    ],
  };
}

export function buildModuleInsight(
  bundle: RepoBundle,
  moduleId: string,
): ModuleInsight | null {
  const labels = bundle.capability.labels;
  const moduleById = new Map(
    bundle.graph.modules.items.map((module) => [module.id, module]),
  );
  const moduleNode = moduleById.get(moduleId);
  if (!moduleNode) return null;

  const topology = bundle.aggregates.dependency_topology.meta.status ===
      "available"
    ? bundle.aggregates.dependency_topology.modules.items.find(
      (row) => row.module_id === moduleId,
    )
    : undefined;
  const topologyAnalysisCoverage = analysisMetric(
    {
      meta: bundle.aggregates.dependency_topology.meta,
      data: bundle.aggregates.dependency_topology.modules,
    },
    labels,
  );
  const structureEdgeCoverage = pageMetric(
    bundle.graph.structure_edges,
    labels,
  );
  const topologyCoverage = topologyAnalysisCoverage.status !== "unavailable"
    ? topologyAnalysisCoverage
    : structureEdgeCoverage.status !== "unavailable"
    ? structureEdgeCoverage
    : topologyAnalysisCoverage;
  const cycleRows = bundle.aggregates.dependency_topology.meta.status ===
      "available"
    ? bundle.aggregates.dependency_topology.cycles.items
    : [];
  const cycles = cycleRows
    .filter((cycle) => cycle.module_ids.includes(moduleId))
    .map(
      (cycle): ModuleCycle => ({
        id: cycle.id,
        moduleIds: [...cycle.module_ids],
        modules: cycle.module_ids.flatMap((id) => {
          const cycleModule = moduleById.get(id);
          return cycleModule ? [cycleModule] : [];
        }),
      }),
    )
    .sort((left, right) => compareText(left.id, right.id));

  const dependencyEdges = structureEdgeCoverage.status === "unavailable"
    ? []
    : bundle.graph.structure_edges.items.filter(
      (edge) => edge.relation === "depends_on",
    );
  const dependencies = uniqueSortedModules(
    dependencyEdges
      .filter((edge) => edge.source === moduleId)
      .flatMap((edge) => {
        const dependency = moduleById.get(edge.target);
        return dependency ? [dependency] : [];
      }),
  );
  const dependents = uniqueSortedModules(
    dependencyEdges
      .filter((edge) => edge.target === moduleId)
      .flatMap((edge) => {
        const dependent = moduleById.get(edge.source);
        return dependent ? [dependent] : [];
      }),
  );

  const hotspotRow = bundle.aggregates.hotspot_quadrant.meta.status ===
      "available"
    ? bundle.aggregates.hotspot_quadrant.data.items.find(
      (row) => row.module_id === moduleId,
    )
    : undefined;
  const ownershipRow = bundle.aggregates.ownership.meta.status === "available" &&
      bundle.capability.views.ownership.status === "available"
    ? bundle.aggregates.ownership.modules.items.find(
      (row) => row.module_id === moduleId,
    )
    : undefined;
  const apiSurfaceRows = bundle.aggregates.api_surface.meta.status === "available"
    ? bundle.aggregates.api_surface.data.items
    : [];
  const rankedApiSurface = [...apiSurfaceRows].sort(
    (left, right) =>
      right.dependent_count - left.dependent_count ||
      compareText(
        modulePath(moduleById, left.module_id),
        modulePath(moduleById, right.module_id),
      ) ||
      compareText(left.module_id, right.module_id),
  );
  const apiSurfaceIndex = rankedApiSurface.findIndex(
    (row) => row.module_id === moduleId,
  );
  const apiSurfaceRow = apiSurfaceIndex === -1
    ? undefined
    : rankedApiSurface[apiSurfaceIndex];

  const commitById = new Map(
    bundle.graph.commits.items.map((commit) => [commit.id, commit]),
  );
  const historyNavigationCoverage = bundle.graph.history_navigation.by_module;
  const navigation = historyNavigationCoverage.items.find(
    (row) => row.module_id === moduleId,
  );
  const history = navigation
    ? pageMetric(navigation.commits, labels)
    : missingHistoryNavigationMetric(historyNavigationCoverage, labels);
  const recentCommits = (navigation?.commits.items ?? [])
    .flatMap((commitId) => {
      const commit = commitById.get(commitId);
      return commit ? [commit] : [];
    })
    .sort(
      (left, right) =>
        compareText(right.committed_at, left.committed_at) ||
        compareText(left.sha, right.sha),
    )
    .slice(0, RECENT_COMMIT_LIMIT);

  const couplingRows = bundle.aggregates.hidden_coupling.meta.status ===
      "available"
    ? bundle.aggregates.hidden_coupling.data.items
    : [];
  const couplingState = analysisMetric(
    bundle.aggregates.hidden_coupling,
    labels,
  );
  const couplings = couplingRows
    .flatMap((row): ModuleCoupling[] => {
      const otherId = row.left_module_id === moduleId
        ? row.right_module_id
        : row.right_module_id === moduleId
        ? row.left_module_id
        : null;
      if (!otherId) return [];
      const otherModule = moduleById.get(otherId);
      return otherModule
        ? [
          {
            module: otherModule,
            cochangeCount: row.cochange_count,
            support: row.support,
          },
        ]
        : [];
    })
    .sort(
      (left, right) =>
        right.cochangeCount - left.cochangeCount ||
        right.support - left.support ||
        compareModules(left.module, right.module),
    );

  const historyCoverage = navigation
    ? pageEvidence("History navigation", navigation.commits, labels)
    : {
      label: "History navigation",
      value: historyNavigationCoverage.disclosure.status === "truncated" ||
          historyNavigationCoverage.next_cursor != null
        ? historyNavigationCoverage.disclosure.reason ??
          "The by-module history-navigation page was truncated before reaching this module."
        : "No module history-navigation row was captured.",
    };
  const analysisWindows = [
    bundle.aggregates.hotspot_quadrant.meta.status === "available"
      ? `${bundle.capability.views.hotspot_quadrant.label}: ${
        formatWindow(
          bundle.aggregates.hotspot_quadrant.meta.window,
          bundle.meta.snapshot.ingested_at,
        )
      }`
      : null,
    bundle.aggregates.hidden_coupling.meta.status === "available"
      ? `${bundle.capability.views.hidden_coupling.label}: ${
        formatWindow(
          bundle.aggregates.hidden_coupling.meta.window,
          bundle.meta.snapshot.ingested_at,
        )
      }`
      : null,
  ].filter((value): value is string => value !== null);
  const couplingCoverage = bundle.aggregates.hidden_coupling.meta.status ===
      "available"
    ? pageEvidence(
      `${bundle.capability.views.hidden_coupling.label} coverage`,
      bundle.aggregates.hidden_coupling.data,
      labels,
    )
    : {
      label: `${bundle.capability.views.hidden_coupling.label} coverage`,
      value: labels.unavailable,
      detail:
        bundle.aggregates.hidden_coupling.meta.unavailable_reason ??
          "The analysis was not produced.",
    };

  return {
    module: moduleNode,
    topology: {
      fanIn: topology?.fan_in ?? dependents.length,
      fanOut: topology?.fan_out ?? dependencies.length,
      coverage: topologyCoverage,
      cycleIds: cycles.map((cycle) => cycle.id),
      cycles,
    },
    hotspot: hotspotRow
      ? {
        commitCount: hotspotRow.commit_count,
        fanIn: hotspotRow.fan_in,
        quadrant: hotspotRow.quadrant,
      }
      : null,
    ownership: ownershipRow
      ? {
        commitCount: ownershipRow.commit_count,
        authorConcentration:
          ownershipRow.author_concentration.status === "available"
            ? ownershipRow.author_concentration.value
            : null,
        busFactor: ownershipRow.bus_factor.status === "available"
          ? ownershipRow.bus_factor.value
          : null,
        authors: [...ownershipRow.authors.items],
      }
      : null,
    apiSurface: apiSurfaceRow
      ? {
        dependentCount: apiSurfaceRow.dependent_count,
        rank: apiSurfaceIndex + 1,
      }
      : null,
    dependencies,
    dependents,
    history,
    recentCommits,
    couplings,
    couplingState,
    evidence: [
      {
        label: "Analysis window",
        value: analysisWindows.length > 0
          ? `${analysisWindows.join("; ")}.`
          : labels.unavailable,
        detail:
          "Topology and ownership use their separately declared bundle windows.",
      },
      {
        label: "Snapshot",
        value:
          `${bundle.meta.snapshot.head_sha} at ${bundle.meta.snapshot.ingested_at}`,
      },
      pageEvidence(
        "Structure-edge coverage",
        bundle.graph.structure_edges,
        labels,
      ),
      historyCoverage,
      sourceRoleEvidence(),
      couplingCoverage,
    ],
  };
}

export interface RepositoryModuleMatches {
  items: RepoModule[];
  total: number;
  bound: number;
}

export function findRepositoryModules(
  bundle: RepoBundle,
  query: string,
  limit = 8,
): RepositoryModuleMatches {
  const normalizedQuery = query.trim().toLowerCase();
  const normalizedLimit = Math.max(0, Math.floor(limit));
  if (!normalizedQuery || normalizedLimit === 0) {
    return { items: [], total: 0, bound: normalizedLimit };
  }

  const score = (module: RepoModule): number | null => {
    const sourcePath = module.source_path.toLowerCase();
    const modulePath = module.module_path.toLowerCase();
    const name = module.name.toLowerCase();
    const pathParts = sourcePath.split("/");
    const basename = pathParts[pathParts.length - 1] ?? sourcePath;
    if ([sourcePath, modulePath, name, basename].includes(normalizedQuery)) {
      return 0;
    }
    if (
      [sourcePath, modulePath, name, basename].some((value) =>
        value.startsWith(normalizedQuery)
      )
    ) {
      return 1;
    }
    if (
      sourcePath.includes(`/${normalizedQuery}`) ||
      modulePath.includes(`::${normalizedQuery}`)
    ) {
      return 2;
    }
    if (
      [sourcePath, modulePath, name].some((value) =>
        value.includes(normalizedQuery)
      )
    ) {
      return 3;
    }
    return null;
  };

  const matches = bundle.graph.modules.items
    .flatMap((module) => {
      const relevance = score(module);
      return relevance == null ? [] : [{ module, relevance }];
    })
    .sort(
      (left, right) =>
        left.relevance - right.relevance ||
        left.module.source_path.length - right.module.source_path.length ||
        compareModules(left.module, right.module),
    );
  return {
    items: matches.slice(0, normalizedLimit).map(({ module }) => module),
    total: matches.length,
    bound: normalizedLimit,
  };
}
