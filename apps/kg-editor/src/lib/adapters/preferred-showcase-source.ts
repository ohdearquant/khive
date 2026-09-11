import {
  loadStaticShowcaseBundle,
  parseBoundedShowcaseResponse,
  type ShowcaseFetch,
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
   * When absent and a curated asset exists, the asset is loaded directly so
   * the browser does not probe a protected route it cannot authenticate to.
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

export class ShowcaseAnalysisNotFoundError extends Error {
  readonly canonicalUrl: string;

  constructor(canonicalUrl: string) {
    super("The configured repository analysis is not available.");
    this.name = "ShowcaseAnalysisNotFoundError";
    this.canonicalUrl = canonicalUrl;
  }
}

// A connection that never resolves, or a response body that stops
// delivering bytes without rejecting, would otherwise leave the snapshot
// read pending forever and keep the page in its loading state. This
// deadline bounds the connection, header wait, and full body parse
// together. Per ADR-147 Amendment 3 only a 404 may fall back to the
// curated static asset, so an elapsed deadline is reported as a hard
// failure rather than silently serving stale static data.
export const DB_SNAPSHOT_TIMEOUT_MS = 5_000;

export async function loadPreferredShowcaseBundle(
  entry: ShowcaseRegistryEntry,
  fetchBundle: ShowcaseFetch = fetch,
  options: PreferredShowcaseOptions = {},
): Promise<LoadedShowcaseBundle> {
  if (!isAllowedShowcaseAnalysis(entry)) {
    if (!entry.assetPath) {
      throw new ShowcaseAnalysisNotFoundError(entry.canonicalUrl);
    }
    return {
      bundle: await loadStaticShowcaseBundle(entry, fetchBundle),
      source: "curated-static-fallback",
    };
  }

  // The snapshot route intentionally hides from unauthenticated callers with
  // a 404. Avoid that guaranteed failed request when the curated fallback can
  // satisfy the load immediately. Dynamic-only entries still probe the route
  // so public deployments and an explicit not-found result keep working.
  if (!options.accessToken?.trim() && entry.assetPath) {
    return {
      bundle: await loadStaticShowcaseBundle(entry, fetchBundle),
      source: "curated-static-fallback",
    };
  }

  const controller = new AbortController();
  const timeoutId = setTimeout(
    () => controller.abort(),
    DB_SNAPSHOT_TIMEOUT_MS,
  );
  const deadline = new Promise<never>((_, reject) => {
    controller.signal.addEventListener("abort", () => {
      reject(
        new Error(
          `Database snapshot request did not settle within ${DB_SNAPSHOT_TIMEOUT_MS}ms.`,
        ),
      );
    }, { once: true });
  });

  const read = readDbSnapshotBundle(entry, fetchBundle, options, controller.signal);
  // When the deadline wins the race the abandoned read may still reject
  // later (e.g. an AbortError from the fetch); mark it handled so it never
  // surfaces as an unhandled rejection.
  read.catch(() => {});
  try {
    return await Promise.race([read, deadline]);
  } finally {
    clearTimeout(timeoutId);
  }
}

async function readDbSnapshotBundle(
  entry: ShowcaseRegistryEntry,
  fetchBundle: ShowcaseFetch,
  options: PreferredShowcaseOptions,
  signal: AbortSignal,
): Promise<LoadedShowcaseBundle> {
  const endpoint = `/api/showcase/analyses/${entry.analysisId}`;
  const accessToken = options.accessToken?.trim();
  const response = await fetchBundle(endpoint, {
    cache: "no-store",
    credentials: "same-origin",
    redirect: "error",
    signal,
    ...(accessToken
      ? { headers: { authorization: `Bearer ${accessToken}` } }
      : {}),
  });

  if (response.status === 404) {
    if (!entry.assetPath) {
      throw new ShowcaseAnalysisNotFoundError(entry.canonicalUrl);
    }
    return {
      bundle: await loadStaticShowcaseBundle(entry, fetchBundle),
      source: "curated-static-fallback",
    };
  }
  if (!response.ok) {
    throw new Error(
      `Database snapshot could not be loaded (HTTP ${response.status}).`,
    );
  }
  if (
    response.headers.get("x-khive-analysis-source") !== "khive-db-snapshot" ||
    response.headers.get("x-khive-analysis-id") !== entry.analysisId
  ) {
    throw new Error(
      "Database snapshot provenance did not match the curated registry.",
    );
  }

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
    throw new Error(
      "Database snapshot repository identity did not match the curated registry.",
    );
  }

  return {
    bundle,
    source: "khive-db-snapshot",
  };
}
