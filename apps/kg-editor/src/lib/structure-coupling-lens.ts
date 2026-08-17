import type { RepoBundle } from "@/lib/repo-bundle";

const pairSeparator = "\u0000";

type CouplingPage = RepoBundle["aggregates"]["hidden_coupling"]["data"];
type CouplingAnalysisStatus = RepoBundle["aggregates"]["hidden_coupling"]["meta"]["status"];
type StructureEdgePage = RepoBundle["graph"]["structure_edges"];

export type CouplingDependencyEvidence = "present" | "absent" | "unknown";

export type StructureCouplingPair = Readonly<{
  key: string;
  leftModuleId: string;
  rightModuleId: string;
  cochangeCount: number;
  support: number;
  dependencyEvidence: CouplingDependencyEvidence;
}>;

export type StructureCouplingLens = Readonly<{
  pairs: readonly StructureCouplingPair[];
  capturedVisiblePairCount: number;
  capturedPairCount: number;
  declaredPairCount: number | null;
  coverage: CouplingPage["disclosure"]["status"];
  coverageReason: string | null;
  dependencyCoverageReason: string | null;
}>;

function undirectedPairKey(left: string, right: string): string {
  return left.localeCompare(right) <= 0
    ? `${left}${pairSeparator}${right}`
    : `${right}${pairSeparator}${left}`;
}

export function buildStructureCouplingLens({
  pairPage,
  structureEdgePage,
  visibleModuleIds,
  limit,
  analysisStatus,
  analysisUnavailableReason,
}: Readonly<{
  pairPage: CouplingPage;
  structureEdgePage: StructureEdgePage;
  visibleModuleIds: ReadonlySet<string>;
  limit: number;
  analysisStatus: CouplingAnalysisStatus;
  analysisUnavailableReason?: string | null;
}>): StructureCouplingLens {
  const boundedLimit = Math.max(0, Math.floor(limit));
  const analysisUnavailable = analysisStatus === "unavailable";
  const structureEvidenceComplete =
    structureEdgePage.disclosure.status === "complete" &&
    !structureEdgePage.truncated &&
    structureEdgePage.next_cursor == null;
  const capturedDependencies = new Set(
    structureEdgePage.items
      .filter((edge) => edge.relation === "depends_on")
      .map((edge) => undirectedPairKey(edge.source, edge.target)),
  );
  const visiblePairs = analysisUnavailable ? [] : pairPage.items
    .filter((pair) =>
      visibleModuleIds.has(pair.left_module_id) &&
      visibleModuleIds.has(pair.right_module_id)
    )
    .sort((left, right) =>
      right.cochange_count - left.cochange_count ||
      right.support - left.support ||
      left.left_module_id.localeCompare(right.left_module_id) ||
      left.right_module_id.localeCompare(right.right_module_id)
    );
  const pairEvidenceUnavailable = analysisUnavailable ||
    pairPage.disclosure.status === "unavailable";
  const pairEvidenceIncomplete = !pairEvidenceUnavailable &&
    (pairPage.disclosure.status === "truncated" ||
      pairPage.truncated ||
      pairPage.next_cursor != null);
  const coverage = pairEvidenceUnavailable
    ? "unavailable"
    : pairEvidenceIncomplete
    ? "truncated"
    : "complete";
  const coverageReason = pairPage.disclosure.reason ??
    (analysisUnavailable
      ? (analysisUnavailableReason ??
        "The hidden-coupling analysis is unavailable.")
      : pairPage.next_cursor != null
      ? "The captured hidden-coupling page has a continuation cursor."
      : pairPage.truncated
      ? "The captured hidden-coupling page is truncated."
      : null);

  return {
    pairs: visiblePairs.slice(0, boundedLimit).map((pair) => {
      const key = undirectedPairKey(pair.left_module_id, pair.right_module_id);
      return {
        key,
        leftModuleId: pair.left_module_id,
        rightModuleId: pair.right_module_id,
        cochangeCount: pair.cochange_count,
        support: pair.support,
        dependencyEvidence: structureEvidenceComplete
          ? capturedDependencies.has(key) ? "present" : "absent"
          : "unknown",
      };
    }),
    capturedVisiblePairCount: visiblePairs.length,
    capturedPairCount: analysisUnavailable ? 0 : pairPage.items.length,
    declaredPairCount: analysisUnavailable ? null : (
      pairPage.total_count.status === "available"
        ? pairPage.total_count.value
        : null
    ),
    coverage,
    coverageReason,
    dependencyCoverageReason: structureEvidenceComplete
      ? null
      : structureEdgePage.disclosure.reason ??
        "The captured structure-edge page is incomplete.",
  };
}
