import type { ViewId } from "@/lib/repo-bundle";

export const REPOSITORY_VIEW_IDS = [
  "structure_graph",
  "history_structure_navigation",
  "dependency_topology",
  "hotspot_quadrant",
  "hidden_coupling",
  "structure_treemap",
  "cadence_timeline",
  "ownership",
  "api_surface",
  "scorecard",
] as const satisfies readonly ViewId[];

const LOCATION_PARAMETERS = [
  "repo",
  "at",
  "module",
  "view",
  "pkg",
  "lens",
  "pair",
] as const;
const VIEW_IDS = new Set<string>(REPOSITORY_VIEW_IDS);
const SNAPSHOT_SHA = /^[0-9a-f]{40}$/;
const MODULE_PATH_LIMIT = 1_024;
const PACKAGE_NAME_LIMIT = 256;
const REPOSITORY_URL_LIMIT = 2_048;

type LocationParameter = (typeof LOCATION_PARAMETERS)[number];

export type StructureGraphLens = "structure" | "hidden_coupling";

export type StructureGraphLocation = Readonly<{
  packageName: string | null;
  lens: StructureGraphLens;
  couplingPair: readonly [string, string] | null;
}>;

export const DEFAULT_STRUCTURE_GRAPH_LOCATION: StructureGraphLocation = {
  packageName: null,
  lens: "structure",
  couplingPair: null,
};

export type RepositoryLocation = Readonly<{
  repository: string | null;
  snapshotSha: string | null;
  modulePath: string | null;
  view: ViewId | null;
  structureGraph: StructureGraphLocation;
}>;

export type RepositoryLocationIssue = Readonly<{
  parameter: LocationParameter;
  message: string;
}>;

export type ParsedRepositoryLocation = Readonly<{
  location: RepositoryLocation;
  issues: readonly RepositoryLocationIssue[];
}>;

export function publicRepositoryUrlIssue(value: string): string | null {
  if (value.length > REPOSITORY_URL_LIMIT) {
    return "The repository URL is too long.";
  }
  try {
    const repository = new URL(value);
    if (
      (repository.protocol !== "https:" && repository.protocol !== "http:") ||
      repository.username ||
      repository.password
    ) {
      return "The repository must be a public HTTP or HTTPS URL.";
    }
  } catch {
    return "The repository must be a public HTTP or HTTPS URL.";
  }
  return null;
}

export function addressableModulePathIssue(value: string): string | null {
  const invalidSegment = value.split("/").some((segment) =>
    !segment || segment === "." || segment === ".."
  );
  if (
    value.length > MODULE_PATH_LIMIT ||
    value.startsWith("/") ||
    value.startsWith("\\") ||
    value.includes("\\") ||
    invalidSegment ||
    /[\u0000-\u001f\u007f]/u.test(value)
  ) {
    return "The module must be a bounded repository-relative source path.";
  }
  return null;
}

function addressablePackageNameIssue(value: string): string | null {
  if (
    value.length > PACKAGE_NAME_LIMIT ||
    /[\u0000-\u001f\u007f]/u.test(value)
  ) {
    return "The package must be a bounded package name.";
  }
  return null;
}

export function canonicalCouplingPair(
  left: string,
  right: string,
): readonly [string, string] {
  return left < right ? [left, right] : [right, left];
}

function singleParameter(
  url: URL,
  parameter: LocationParameter,
  issues: RepositoryLocationIssue[],
): string | null {
  const values = url.searchParams.getAll(parameter);
  if (values.length === 0) return null;
  if (values.length > 1) {
    issues.push({
      parameter,
      message: `The ${parameter} parameter must appear at most once.`,
    });
    return null;
  }
  if (!values[0]) {
    issues.push({
      parameter,
      message: `The ${parameter} parameter cannot be empty.`,
    });
    return null;
  }
  return values[0];
}

function parseRepository(
  value: string | null,
  issues: RepositoryLocationIssue[],
): string | null {
  if (value == null) return null;
  const message = publicRepositoryUrlIssue(value);
  if (message) {
    issues.push({ parameter: "repo", message });
    return null;
  }
  return value;
}

function parseSnapshotSha(
  value: string | null,
  issues: RepositoryLocationIssue[],
): string | null {
  if (value == null) return null;
  if (!SNAPSHOT_SHA.test(value)) {
    issues.push({
      parameter: "at",
      message: "The snapshot must be a lowercase 40-character Git SHA.",
    });
    return null;
  }
  return value;
}

function parseModulePath(
  value: string | null,
  issues: RepositoryLocationIssue[],
): string | null {
  if (value == null) return null;
  const message = addressableModulePathIssue(value);
  if (message) {
    issues.push({ parameter: "module", message });
    return null;
  }
  return value;
}

function parseView(
  value: string | null,
  issues: RepositoryLocationIssue[],
): ViewId | null {
  if (value == null) return null;
  if (!VIEW_IDS.has(value)) {
    issues.push({
      parameter: "view",
      message: "The requested analysis view is not supported.",
    });
    return null;
  }
  return value as ViewId;
}

function parsePackageName(
  value: string | null,
  issues: RepositoryLocationIssue[],
): string | null {
  if (value == null) return null;
  const message = addressablePackageNameIssue(value);
  if (message) {
    issues.push({ parameter: "pkg", message });
    return null;
  }
  return value;
}

function parseLens(
  value: string | null,
  issues: RepositoryLocationIssue[],
): StructureGraphLens {
  if (value == null) return "structure";
  if (value !== "structure" && value !== "hidden_coupling") {
    issues.push({
      parameter: "lens",
      message: "The requested Structure Graph lens is not supported.",
    });
    return "structure";
  }
  return value;
}

function parseCouplingPair(
  url: URL,
  issues: RepositoryLocationIssue[],
): readonly [string, string] | null {
  const values = url.searchParams.getAll("pair");
  if (values.length === 0) return null;
  if (values.length !== 2) {
    issues.push({
      parameter: "pair",
      message: "A coupling pair must contain exactly two module source paths.",
    });
    return null;
  }
  const pathIssue = values.find((value) => addressableModulePathIssue(value));
  if (pathIssue !== undefined) {
    issues.push({
      parameter: "pair",
      message: "Each coupling endpoint must be a bounded repository-relative source path.",
    });
    return null;
  }
  if (values[0] === values[1]) {
    issues.push({
      parameter: "pair",
      message: "A coupling pair must name two distinct module source paths.",
    });
    return null;
  }
  return canonicalCouplingPair(values[0], values[1]);
}

function parseStructureGraphLocation(
  url: URL,
  view: ViewId | null,
  snapshotCanFocusPair: boolean,
  issues: RepositoryLocationIssue[],
): StructureGraphLocation {
  const packageValue = singleParameter(url, "pkg", issues);
  const lensValue = singleParameter(url, "lens", issues);
  const requestedPair = parseCouplingPair(url, issues);
  const graphParameters = ["pkg", "lens", "pair"] as const;

  if (view !== "structure_graph") {
    for (const parameter of graphParameters) {
      if (url.searchParams.has(parameter)) {
        issues.push({
          parameter,
          message: `The ${parameter} parameter is only supported by the Structure Graph view.`,
        });
      }
    }
    return DEFAULT_STRUCTURE_GRAPH_LOCATION;
  }

  const lens = parseLens(lensValue, issues);
  const packageName = parsePackageName(packageValue, issues);
  let couplingPair = requestedPair;
  if (couplingPair && lens !== "hidden_coupling") {
    issues.push({
      parameter: "pair",
      message: "A focused coupling pair requires the hidden_coupling lens.",
    });
    couplingPair = null;
  }
  if (requestedPair && url.searchParams.has("pkg") && packageName === null) {
    issues.push({
      parameter: "pair",
      message:
        "A focused coupling pair requires a valid, unambiguous package scope.",
    });
    couplingPair = null;
  }
  if (requestedPair && !snapshotCanFocusPair) {
    issues.push({
      parameter: "pair",
      message: "A focused coupling pair requires a valid, unambiguous snapshot.",
    });
    couplingPair = null;
  }
  return {
    packageName,
    lens,
    couplingPair,
  };
}

export function parseRepositoryLocation(url: URL): ParsedRepositoryLocation {
  const issues: RepositoryLocationIssue[] = [];
  const repository = singleParameter(url, "repo", issues);
  const snapshotSha = singleParameter(url, "at", issues);
  const modulePath = singleParameter(url, "module", issues);
  const view = singleParameter(url, "view", issues);
  const parsedView = parseView(view, issues);
  const parsedSnapshotSha = parseSnapshotSha(snapshotSha, issues);

  return {
    location: {
      repository: parseRepository(repository, issues),
      snapshotSha: parsedSnapshotSha,
      modulePath: parseModulePath(modulePath, issues),
      view: parsedView,
      structureGraph: parseStructureGraphLocation(
        url,
        parsedView,
        !url.searchParams.has("at") || parsedSnapshotSha !== null,
        issues,
      ),
    },
    issues,
  };
}

export function repositoryLocationUrl(
  base: URL,
  location: RepositoryLocation,
): URL {
  const url = new URL(base.origin + base.pathname);
  const values: Record<LocationParameter, string | null> = {
    repo: location.repository,
    at: location.snapshotSha,
    module: location.modulePath,
    view: location.view,
    // Structure-graph parameters are conditional on the view and emitted by
    // the dedicated block below; null here so the closed-order loop skips them.
    pkg: null,
    lens: null,
    pair: null,
  };
  for (const parameter of LOCATION_PARAMETERS) {
    const value = values[parameter];
    if (value) url.searchParams.append(parameter, value);
  }
  if (location.view === "structure_graph") {
    if (location.structureGraph.packageName) {
      url.searchParams.append("pkg", location.structureGraph.packageName);
    }
    if (location.structureGraph.lens === "hidden_coupling") {
      url.searchParams.append("lens", location.structureGraph.lens);
      if (location.structureGraph.couplingPair) {
        for (const endpoint of canonicalCouplingPair(
          location.structureGraph.couplingPair[0],
          location.structureGraph.couplingPair[1],
        )) {
          url.searchParams.append("pair", endpoint);
        }
      }
    }
  }
  return url;
}
