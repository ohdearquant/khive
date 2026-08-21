import {
  loadStaticShowcaseBundle,
  parseBoundedShowcaseResponse,
  type ShowcaseFetch,
  type ShowcaseResponse,
} from "@/lib/adapters/static-showcase-source";
import type { RepoBundle } from "@/lib/repo-bundle";
import {
  isAllowedShowcaseAnalysis,
  normalizeRepositoryUrl,
  type ShowcaseRegistryEntry,
} from "@/lib/showcase-registry";

export type ShowcaseBundleSource =
  | "khive-db-snapshot"
  | "curated-static-fallback";

export type LoadedShowcaseBundle = Readonly<{
  bundle: RepoBundle;
  source: ShowcaseBundleSource;
}>;

export type PreferredShowcaseOptions = Readonly<{
  /**
   * Operator-supplied bearer token for the protected DB-snapshot routes.
   * When absent, the request is sent without credentials and the protected
   * route fails closed to 404, which selects the curated static fallback.
   */
  accessToken?: string | null;
}>;

/**
 * Browser storage key an operator uses to unlock DB-backed snapshots in the
 * UI: `sessionStorage.setItem(SHOWCASE_ACCESS_TOKEN_STORAGE_KEY, token)`.
 */
export const SHOWCASE_ACCESS_TOKEN_STORAGE_KEY = "khive.showcase.accessToken";

export function readOperatorShowcaseAccessToken(): string | null {
  if (typeof window === "undefined") return null;
  try {
    return window.sessionStorage.getItem(SHOWCASE_ACCESS_TOKEN_STORAGE_KEY);
  } catch {
    return null;
  }
}

export async function loadPreferredShowcaseBundle(
  entry: ShowcaseRegistryEntry,
  fetchBundle: ShowcaseFetch = fetch,
  options: PreferredShowcaseOptions = {},
): Promise<LoadedShowcaseBundle> {
  if (!isAllowedShowcaseAnalysis(entry)) {
    return {
      bundle: await loadStaticShowcaseBundle(entry, fetchBundle),
      source: "curated-static-fallback",
    };
  }

  const dbBundle = await tryLoadDbSnapshotBundle(entry, fetchBundle, options);
  if (dbBundle) {
    return { bundle: dbBundle, source: "khive-db-snapshot" };
  }

  return {
    bundle: await loadStaticShowcaseBundle(entry, fetchBundle),
    source: "curated-static-fallback",
  };
}

// A connection that never resolves, or a response body that stops
// delivering bytes without rejecting, would otherwise leave the snapshot
// read pending forever and keep the page in its loading state instead of
// reaching the static fallback below. This deadline bounds the connection,
// header wait, and full body parse together, so any of those hangs falls
// back to the curated static asset once it elapses.
export const DB_SNAPSHOT_TIMEOUT_MS = 5_000;

// The DB snapshot is a progressive enhancement over the static asset: any
// failure on this path (network, status, provenance, schema, identity, or
// timeout) must fall back to the static render rather than fail the page.
async function tryLoadDbSnapshotBundle(
  entry: ShowcaseRegistryEntry,
  fetchBundle: ShowcaseFetch,
  options: PreferredShowcaseOptions,
): Promise<RepoBundle | null> {
  const controller = new AbortController();
  const timeoutId = setTimeout(
    () => controller.abort(),
    DB_SNAPSHOT_TIMEOUT_MS,
  );
  const deadline = new Promise<null>((resolve) => {
    controller.signal.addEventListener("abort", () => resolve(null), {
      once: true,
    });
  });

  try {
    return await Promise.race([
      readDbSnapshotBundle(entry, fetchBundle, options, controller.signal),
      deadline,
    ]);
  } finally {
    clearTimeout(timeoutId);
  }
}

async function readDbSnapshotBundle(
  entry: ShowcaseRegistryEntry,
  fetchBundle: ShowcaseFetch,
  options: PreferredShowcaseOptions,
  signal: AbortSignal,
): Promise<RepoBundle | null> {
  const endpoint = `/api/showcase/analyses/${entry.analysisId}`;
  const accessToken = options.accessToken?.trim();
  let response: ShowcaseResponse;
  try {
    response = await fetchBundle(endpoint, {
      cache: "no-store",
      credentials: "same-origin",
      redirect: "error",
      signal,
      ...(accessToken
        ? { headers: { authorization: `Bearer ${accessToken}` } }
        : {}),
    });
  } catch {
    return null;
  }

  if (!response.ok) {
    return null;
  }
  if (
    response.headers.get("x-khive-analysis-source") !== "khive-db-snapshot" ||
    response.headers.get("x-khive-analysis-id") !== entry.analysisId
  ) {
    return null;
  }

  try {
    const bundle = await parseBoundedShowcaseResponse(
      response,
      "Database snapshot",
    );
    const expectedRepository = normalizeRepositoryUrl(entry.canonicalUrl);
    const actualRepository = normalizeRepositoryUrl(
      bundle.meta.repository.canonical_url,
    );
    if (
      !expectedRepository.ok ||
      !actualRepository.ok ||
      actualRepository.value !== expectedRepository.value
    ) {
      return null;
    }
    return bundle;
  } catch {
    return null;
  }
}
