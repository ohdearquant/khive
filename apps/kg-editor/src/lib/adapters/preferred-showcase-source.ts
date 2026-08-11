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

  const endpoint = `/api/showcase/analyses/${entry.analysisId}`;
  const response = await fetchBundle(endpoint, {
    cache: "no-store",
    credentials: "same-origin",
    redirect: "error",
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
