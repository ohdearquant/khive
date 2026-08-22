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
  if (value.length > REPOSITORY_URL_LIMIT) {
    issues.push({ parameter: "repo", message: "The repository URL is too long." });
    return null;
  }
  const normalized = normalizeRepositoryUrl(value);
  if (!normalized.ok) {
    issues.push({ parameter: "repo", message: normalized.reason });
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

// Investigation URLs are copied to the clipboard and written to browser
// history, so they carry ONLY the four parameters this app defines. A page
// URL that arrived with an unrelated query parameter or fragment — a
// credential such as ?access_token=..., a fragment-borne secret, a tracker
// value — is never copied forward: the URL is rebuilt from the origin and
// path only, and every other parameter and the hash are discarded. The same
// guarantee extends to the repo VALUE itself: the writer emits either a
// value this application's validator (`normalizeRepositoryUrl`) produced, or
// no `repo` param at all — there is no third case where an unvalidated or
// partially-sanitized value reaches history. A path segment that survives
// canonicalization is, by definition, the repository identity, and is
// written as-is; a credential placed in a path segment (rather than
// authority userinfo, which the validator rejects) is out of scope by
// policy, not by accident. `parseRepositoryLocation` never sees this
// history-only form — it always parses the original, unmodified value, so
// validation results are unaffected by this guarantee.
function historySafeRepositoryValue(value: string): string | null {
  const normalized = normalizeRepositoryUrl(value);
  return normalized.ok ? normalized.value : null;
}

export function repositoryLocationUrl(
  base: URL,
  location: RepositoryLocation,
): URL {
  const url = new URL(base.origin + base.pathname);
  const values: Record<LocationParameter, string | null> = {
    repo: location.repository === null
      ? null
      : historySafeRepositoryValue(location.repository),
    at: location.snapshotSha,
    module: location.modulePath,
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
 * already discards every foreign query parameter and the fragment, and
 * strips a repository value's own nested query/fragment before writing it
 * to history. This share form applies the same value boundary more
 * strictly still: the shared repository value is normalized to origin +
 * pathname (not just truncated at the first delimiter), and a module path
 * carrying a URL query or fragment delimiter is omitted entirely rather
 * than encoded into the value, because encoding preserves — not redacts —
 * whatever the delimiter introduced. `at` and `view` need no value
 * boundary: their parse contracts are a 40-hex SHA and a closed id set.
 */
export function investigationShareUrl(
  base: URL,
  location: RepositoryLocation,
): URL {
  const url = new URL(`${base.origin}${base.pathname}`);
  if (location.repository) {
    const repository = shareSafeRepository(location.repository);
    if (repository) url.searchParams.append("repo", repository);
  }
  if (location.snapshotSha) url.searchParams.append("at", location.snapshotSha);
  if (location.modulePath && !/[?#]/u.test(location.modulePath)) {
    url.searchParams.append("module", location.modulePath);
  }
  if (location.view) url.searchParams.append("view", location.view);
  return url;
}

function shareSafeRepository(value: string): string | null {
  try {
    const repository = new URL(value);
    return `${repository.origin}${repository.pathname}`;
  } catch {
    return null;
  }
}
