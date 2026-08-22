export const SHOWCASE_ASSET_PREFIX = "/showcase/";

export type ShowcaseRegistryEntry = Readonly<{
  id: string;
  canonicalUrl: string;
  aliases: readonly string[];
  assetPath?: string;
  analysisId?: string;
}>;

export type ShowcaseLookup =
  | Readonly<{ status: "hit"; normalizedUrl: string; entry: ShowcaseRegistryEntry }>
  | Readonly<{ status: "miss"; normalizedUrl: string }>
  | Readonly<{ status: "invalid"; reason: string }>;

export const SHOWCASE_REGISTRY: readonly ShowcaseRegistryEntry[] = [
  {
    id: "github.com/ohdearquant/khive",
    canonicalUrl: "https://github.com/ohdearquant/khive",
    aliases: [
      "https://github.com/ohdearquant/khive",
      "http://github.com/ohdearquant/khive",
      "https://www.github.com/ohdearquant/khive",
    ],
    assetPath: "/showcase/khive-repo-v1-khive.json",
    analysisId: "khive",
  },
] as const;

export function normalizeRepositoryUrl(input: string):
  | Readonly<{ ok: true; value: string }>
  | Readonly<{ ok: false; reason: string }> {
  const candidate = input.trim();
  if (!candidate) {
    return { ok: false, reason: "Enter a public repository URL." };
  }

  let url: URL;
  try {
    url = new URL(candidate);
  } catch {
    return { ok: false, reason: "Enter a complete http or https repository URL." };
  }

  if (url.protocol !== "https:" && url.protocol !== "http:") {
    return { ok: false, reason: "Repository URLs must use http or https." };
  }
  if (url.username || url.password) {
    return { ok: false, reason: "Repository URLs cannot contain credentials." };
  }

  let segments: string[];
  try {
    segments = url.pathname
      .split("/")
      .filter(Boolean)
      .map((segment) => decodeURIComponent(segment));
  } catch {
    return { ok: false, reason: "The repository URL contains invalid path encoding." };
  }
  if (segments.length < 2 || segments.some((segment) => segment === "." || segment === "..")) {
    return { ok: false, reason: "The URL must identify a repository owner and name." };
  }

  const repository = segments.at(-1)?.replace(/\.git$/i, "") ?? "";
  if (!repository) {
    return { ok: false, reason: "The URL must include a repository name." };
  }
  segments[segments.length - 1] = repository;

  const host = url.hostname.toLowerCase() === "www.github.com" ? "github.com" : url.hostname.toLowerCase();
  const authority = url.port ? `${host}:${url.port}` : host;
  return { ok: true, value: `https://${authority}/${segments.join("/")}` };
}

export function resolveShowcaseRepository(
  input: string,
  registry: readonly ShowcaseRegistryEntry[] = SHOWCASE_REGISTRY,
): ShowcaseLookup {
  const normalized = normalizeRepositoryUrl(input);
  if (!normalized.ok) return { status: "invalid", reason: normalized.reason };

  for (const entry of registry) {
    const candidates = [entry.canonicalUrl, ...entry.aliases];
    if (candidates.some((candidate) => {
      const normalizedCandidate = normalizeRepositoryUrl(candidate);
      return normalizedCandidate.ok && normalizedCandidate.value === normalized.value;
    })) {
      return { status: "hit", normalizedUrl: normalized.value, entry };
    }
  }

  return { status: "miss", normalizedUrl: normalized.value };
}

export function isAllowedShowcaseAsset(
  assetPath: string | undefined,
  registry: readonly ShowcaseRegistryEntry[] = SHOWCASE_REGISTRY,
): assetPath is string {
  return typeof assetPath === "string" &&
    assetPath.startsWith(SHOWCASE_ASSET_PREFIX) &&
    registry.some((entry) => entry.assetPath === assetPath);
}

export function isAllowedShowcaseAnalysis(
  entry: ShowcaseRegistryEntry,
): entry is ShowcaseRegistryEntry & Readonly<{ analysisId: string }> {
  return typeof entry.analysisId === "string" &&
    /^[a-z0-9](?:[a-z0-9-]{0,62}[a-z0-9])?$/.test(entry.analysisId);
}
