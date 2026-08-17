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

export async function loadPreferredShowcaseBundle(
  entry: ShowcaseRegistryEntry,
  fetchBundle: ShowcaseFetch = fetch,
): Promise<LoadedShowcaseBundle> {
  if (!isAllowedShowcaseAnalysis(entry)) {
    return {
      bundle: await loadStaticShowcaseBundle(entry, fetchBundle),
      source: "curated-static-fallback",
    };
  }

  const dbBundle = await tryLoadDbSnapshotBundle(entry, fetchBundle);
  if (dbBundle) {
    return { bundle: dbBundle, source: "khive-db-snapshot" };
  }

  return {
    bundle: await loadStaticShowcaseBundle(entry, fetchBundle),
    source: "curated-static-fallback",
  };
}

// The DB snapshot is a progressive enhancement over the static asset: any
// failure on this path (network, status, provenance, schema, or identity)
// must fall back to the static render rather than fail the page.
async function tryLoadDbSnapshotBundle(
  entry: ShowcaseRegistryEntry,
  fetchBundle: ShowcaseFetch,
): Promise<RepoBundle | null> {
  const endpoint = `/api/showcase/analyses/${entry.analysisId}`;
  let response: ShowcaseResponse;
  try {
    response = await fetchBundle(endpoint, {
      cache: "no-store",
      credentials: "same-origin",
      redirect: "error",
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
