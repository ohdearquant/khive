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

  const endpoint = `/api/showcase/analyses/${entry.analysisId}`;
  const accessToken = options.accessToken?.trim();
  const response = await fetchBundle(endpoint, {
    cache: "no-store",
    credentials: "same-origin",
    redirect: "error",
    ...(accessToken
      ? { headers: { authorization: `Bearer ${accessToken}` } }
      : {}),
  });

  if (response.status === 404) {
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
