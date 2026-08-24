import type { ViewId } from "@/lib/repo-bundle";
import { normalizeRepositoryUrl } from "@/lib/showcase-registry";

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
  "module_id",
  "view",
] as const;
const VIEW_IDS = new Set<string>(REPOSITORY_VIEW_IDS);
const SNAPSHOT_SHA = /^[0-9a-f]{40}$/;
const MODULE_PATH_LIMIT = 1_024;
const MODULE_ID_LIMIT = 1_024;

type LocationParameter = (typeof LOCATION_PARAMETERS)[number];

export type RepositoryLocation = Readonly<{
  repository: string | null;
  snapshotSha: string | null;
  modulePath: string | null;
  moduleId: string | null;
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
  const normalized = normalizeRepositoryUrl(value);
  return normalized.ok ? null : normalized.reason;
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
  const normalized = normalizeRepositoryUrl(value);
  if (!normalized.ok) {
    issues.push({ parameter: "repo", message: normalized.reason });
    return null;
  }
  return normalized.value;
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

function parseModuleId(
  value: string | null,
  issues: RepositoryLocationIssue[],
): string | null {
  if (value == null) return null;
  if (
    value.length > MODULE_ID_LIMIT ||
    /[\u0000-\u001f\u007f]/u.test(value)
  ) {
    issues.push({
      parameter: "module_id",
      message: "The module identifier must be a bounded printable value.",
    });
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
  const moduleId = singleParameter(url, "module_id", issues);
  const view = singleParameter(url, "view", issues);

  return {
    location: {
      repository: parseRepository(repository, issues),
      snapshotSha: parseSnapshotSha(snapshotSha, issues),
      modulePath: parseModulePath(modulePath, issues),
      moduleId: parseModuleId(moduleId, issues),
      view: parseView(view, issues),
    },
    issues,
  };
}

// Investigation URLs are copied to the clipboard and written to browser
// history, so they carry ONLY the five parameters this app defines. A page
// URL that arrived with an unrelated query parameter or fragment — a
// credential such as ?access_token=..., a fragment-borne secret, a tracker
// value — is never copied forward: the URL is rebuilt from the origin and
// path only, and every other parameter and the hash are discarded.
export function repositoryLocationUrl(
  base: URL,
  location: RepositoryLocation,
): URL {
  const url = new URL(base.origin + base.pathname);
  const values: Record<LocationParameter, string | null> = {
    repo: location.repository ? repositoryOriginAndPathname(location.repository) : null,
    at: location.snapshotSha,
    module: location.modulePath,
    module_id: location.moduleId,
    view: location.view,
  };
  for (const parameter of LOCATION_PARAMETERS) {
    const value = values[parameter];
    if (value) url.searchParams.append(parameter, value);
  }
  return url;
}

/**
 * The shareable form of an investigation URL. `repositoryLocationUrl`
 * already discards every foreign query parameter and the fragment, but a
 * validated repository URL may legitimately carry a query string or
 * fragment of its OWN (deep-link support keeps those in-browser), so this
 * boundary also applies to the parameter VALUES, not just the parameter
 * names: the shared repository value is normalized to origin + pathname,
 * and a module path or identifier carrying a URL query or fragment delimiter
 * is omitted entirely rather than encoded into the value, because encoding
 * preserves — not redacts — whatever the delimiter introduced. `at` and
 * `view` need no value boundary: their parse contracts are a 40-hex SHA and
 * a closed id set.
 */
export function investigationShareUrl(
  base: URL,
  location: RepositoryLocation,
): URL {
  const url = new URL(`${base.origin}${base.pathname}`);
  if (location.repository) {
    const repository = repositoryOriginAndPathname(location.repository);
    if (repository) url.searchParams.append("repo", repository);
  }
  if (location.snapshotSha) url.searchParams.append("at", location.snapshotSha);
  if (location.modulePath && !/[?#]/u.test(location.modulePath)) {
    url.searchParams.append("module", location.modulePath);
  }
  if (location.moduleId && !/[?#]/u.test(location.moduleId)) {
    url.searchParams.append("module_id", location.moduleId);
  }
  if (location.view) url.searchParams.append("view", location.view);
  return url;
}

function repositoryOriginAndPathname(value: string): string | null {
  try {
    const repository = new URL(value);
    return `${repository.origin}${repository.pathname}`;
  } catch {
    return null;
  }
}
