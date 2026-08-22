import { readOperatorShowcaseAccessToken } from "@/lib/adapters/preferred-showcase-source";
import {
  normalizeRepositoryUrl,
  SHOWCASE_REGISTRY,
  type ShowcaseRegistryEntry,
} from "@/lib/showcase-registry";

export const SHOWCASE_CATALOG_MAX_ENTRIES = 64;
export const SHOWCASE_CATALOG_MAX_BYTES = 256 * 1024;

const ANALYSIS_ID = /^[a-z0-9](?:[a-z0-9-]{0,62}[a-z0-9])?$/;
const CATALOG_SCHEMA = "khive.showcase.catalog.v1";

export type ShowcaseAnalysisCatalogEntry = Readonly<{
  analysis_id: string;
  canonical_url: string;
}>;

export type ShowcaseAnalysisCatalogResult =
  | Readonly<{
      status: "ready";
      entries: readonly ShowcaseAnalysisCatalogEntry[];
      message: string;
    }>
  | Readonly<{
      status: "static-only" | "degraded";
      entries: readonly [];
      message: string;
    }>;

export type ShowcaseCatalogFetch = (
  input: string,
  init?: RequestInit,
) => Promise<Pick<Response, "ok" | "status" | "headers" | "arrayBuffer">>;

function exactKeys(value: object, expected: readonly string[]): boolean {
  return Object.keys(value).sort().join(",") === [...expected].sort().join(",");
}

function invalidCatalog(): never {
  throw new Error("The repository analysis catalog is invalid.");
}

export function parseShowcaseAnalysisCatalog(
  value: unknown,
): readonly ShowcaseAnalysisCatalogEntry[] {
  if (
    !value || typeof value !== "object" || Array.isArray(value) ||
    !exactKeys(value, ["schema_version", "entries"]) ||
    Reflect.get(value, "schema_version") !== CATALOG_SCHEMA
  ) {
    return invalidCatalog();
  }

  const candidates = Reflect.get(value, "entries");
  if (
    !Array.isArray(candidates) ||
    candidates.length === 0 ||
    candidates.length > SHOWCASE_CATALOG_MAX_ENTRIES
  ) {
    return invalidCatalog();
  }

  const entries: ShowcaseAnalysisCatalogEntry[] = [];
  const ids = new Set<string>();
  const urls = new Set<string>();
  let previousId: string | undefined;
  for (const candidate of candidates) {
    if (
      !candidate || typeof candidate !== "object" || Array.isArray(candidate) ||
      !exactKeys(candidate, ["analysis_id", "canonical_url"])
    ) {
      return invalidCatalog();
    }
    const analysisId = Reflect.get(candidate, "analysis_id");
    const canonicalUrl = Reflect.get(candidate, "canonical_url");
    if (
      typeof analysisId !== "string" || !ANALYSIS_ID.test(analysisId) ||
      typeof canonicalUrl !== "string"
    ) {
      return invalidCatalog();
    }
    const normalized = normalizeRepositoryUrl(canonicalUrl);
    if (
      !normalized.ok || normalized.value !== canonicalUrl ||
      ids.has(analysisId) || urls.has(normalized.value) ||
      (previousId !== undefined && analysisId < previousId)
    ) {
      return invalidCatalog();
    }
    ids.add(analysisId);
    urls.add(normalized.value);
    previousId = analysisId;
    entries.push({
      analysis_id: analysisId,
      canonical_url: normalized.value,
    });
  }
  return entries;
}

export async function loadShowcaseAnalysisCatalog(
  fetchCatalog: ShowcaseCatalogFetch = fetch,
): Promise<ShowcaseAnalysisCatalogResult> {
  try {
    const accessToken = readOperatorShowcaseAccessToken()?.trim();
    const response = await fetchCatalog("/api/showcase/analyses", {
      cache: "no-store",
      credentials: "same-origin",
      redirect: "error",
      ...(accessToken
        ? { headers: { authorization: `Bearer ${accessToken}` } }
        : {}),
    });
    if (response.status === 404) {
      return {
        status: "static-only",
        entries: [],
        message: "The server analysis catalog is not configured; curated static repositories remain available.",
      };
    }
    if (!response.ok) {
      throw new Error(`catalog HTTP ${response.status}`);
    }

    const declaredLength = Number(response.headers.get("content-length"));
    if (
      Number.isFinite(declaredLength) &&
      declaredLength > SHOWCASE_CATALOG_MAX_BYTES
    ) {
      return invalidCatalog();
    }
    const bytes = new Uint8Array(await response.arrayBuffer());
    if (bytes.byteLength > SHOWCASE_CATALOG_MAX_BYTES) {
      return invalidCatalog();
    }
    const json = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    const entries = parseShowcaseAnalysisCatalog(JSON.parse(json));
    return {
      status: "ready",
      entries,
      message: `${entries.length} configured repository ${entries.length === 1 ? "analysis" : "analyses"} discovered.`,
    };
  } catch {
    return {
      status: "degraded",
      entries: [],
      message: "The server analysis catalog is unavailable; curated static repositories remain available.",
    };
  }
}

export function mergeShowcaseRegistry(
  catalog: readonly ShowcaseAnalysisCatalogEntry[],
  staticRegistry: readonly ShowcaseRegistryEntry[] = SHOWCASE_REGISTRY,
): readonly ShowcaseRegistryEntry[] {
  const merged: ShowcaseRegistryEntry[] = staticRegistry.map((entry) => ({
    ...entry,
    analysisId: undefined,
  }));
  const staticByUrl = new Map<string, number>();
  for (const [index, entry] of merged.entries()) {
    const normalized = normalizeRepositoryUrl(entry.canonicalUrl);
    if (normalized.ok && !staticByUrl.has(normalized.value)) {
      staticByUrl.set(normalized.value, index);
    }
  }

  for (const entry of catalog) {
    const staticIndex = staticByUrl.get(entry.canonical_url);
    if (staticIndex !== undefined) {
      merged[staticIndex] = {
        ...merged[staticIndex],
        analysisId: entry.analysis_id,
      };
      continue;
    }
    merged.push({
      id: `analysis:${entry.analysis_id}`,
      canonicalUrl: entry.canonical_url,
      aliases: [entry.canonical_url],
      assetPath: undefined,
      analysisId: entry.analysis_id,
    });
  }
  return merged;
}
