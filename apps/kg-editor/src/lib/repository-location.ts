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

const LOCATION_PARAMETERS = ["repo", "at", "module", "view"] as const;
const VIEW_IDS = new Set<string>(REPOSITORY_VIEW_IDS);
const SNAPSHOT_SHA = /^[0-9a-f]{40}$/;
const MODULE_PATH_LIMIT = 1_024;
const REPOSITORY_URL_LIMIT = 2_048;

type LocationParameter = (typeof LOCATION_PARAMETERS)[number];

export type RepositoryLocation = Readonly<{
  repository: string | null;
  snapshotSha: string | null;
  modulePath: string | null;
  view: ViewId | null;
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
      repository.password ||
      repository.search ||
      repository.hash
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

export function parseRepositoryLocation(url: URL): ParsedRepositoryLocation {
  const issues: RepositoryLocationIssue[] = [];
  const repository = singleParameter(url, "repo", issues);
  const snapshotSha = singleParameter(url, "at", issues);
  const modulePath = singleParameter(url, "module", issues);
  const view = singleParameter(url, "view", issues);

  return {
    location: {
      repository: parseRepository(repository, issues),
      snapshotSha: parseSnapshotSha(snapshotSha, issues),
      modulePath: parseModulePath(modulePath, issues),
      view: parseView(view, issues),
    },
    issues,
  };
}

export function repositoryLocationUrl(
  base: URL,
  location: RepositoryLocation,
): URL {
  const url = new URL(base);
  for (const parameter of LOCATION_PARAMETERS) {
    url.searchParams.delete(parameter);
  }
  if (location.repository) url.searchParams.append("repo", location.repository);
  if (location.snapshotSha) url.searchParams.append("at", location.snapshotSha);
  if (location.modulePath) {
    url.searchParams.append("module", location.modulePath);
  }
  if (location.view) url.searchParams.append("view", location.view);
  return url;
}
