export const SHOWCASE_ASSET_PREFIX = "/showcase/";
export const REPOSITORY_URL_LIMIT = 2_048;

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
      "khive",
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
  return normalizeRepositoryUrlImpl(input, true);
}

// verifyFixedPoint gates a single guarded re-entry: after building the
// canonical value below, it re-normalizes that value (with
// verifyFixedPoint=false, so the inner call cannot recurse again) and
// requires the result to be ok and byte-identical. A value that is already
// canonical settles in one step, so one re-entry is sufficient to prove the
// emitted identity is closed rather than merely equal to a lossily-mutated
// segment array (which is what let a doubled ".git.git" suffix through
// before this check existed).
function normalizeRepositoryUrlImpl(
  input: string,
  verifyFixedPoint: boolean,
):
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

  const segments = decodedPathSegments(url);
  if (!segments) {
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
  // This join is deliberately unencoded: it is what gives the round-trip
  // check below its meaning (re-encoding the segments here would make the
  // re-parse trivially match, silently reopening the decoded-slash /
  // decoded-backslash / decoded-query-delimiter holes those checks exist
  // to close). The consequence is that a literal "%" in a decoded path
  // segment is genuinely ambiguous under this unencoded join, so it is
  // refused by policy, not by accident. This is a behaviour change from
  // the previous normalizer, which accepted a literal "%" in a repository
  // name.
  const value = `https://${authority}/${segments.join("/")}`;

  if (value.length > REPOSITORY_URL_LIMIT) {
    return { ok: false, reason: "The repository URL is too long." };
  }

  // The canonical value is built by joining DECODED segments back together
  // with "/" — it is not re-encoded. Re-parsing it with the same parser and
  // requiring the segments to come back unchanged is what catches any
  // character that is structural in a URL path (a decoded "/", "?", "#",
  // "\\", …): such a character would shift where a segment boundary falls
  // on the next parse, so the round trip fails and the input is rejected.
  // This subsumes any specific-character blacklist without enumerating one.
  let reparsed: URL;
  try {
    reparsed = new URL(value);
  } catch {
    return { ok: false, reason: "The repository URL contains invalid path encoding." };
  }
  const reparsedSegments = decodedPathSegments(reparsed);
  if (
    !reparsedSegments ||
    reparsedSegments.length !== segments.length ||
    reparsedSegments.some((segment, index) => segment !== segments[index])
  ) {
    return { ok: false, reason: "The repository URL contains invalid path encoding." };
  }

  // Runtime fixed-point invariant: the check above only proves the segment
  // array survives a re-parse — it runs AFTER the terminal ".git" strip has
  // already mutated that array, so it cannot see whether the canonical
  // VALUE is closed under normalization (a doubled "repo.git.git" strips to
  // "repo.git" here, which trivially round-trips as a segment array, but is
  // not itself canonical). Re-normalizing the canonical value and requiring
  // an identical result is what actually proves closure, on every input,
  // not just the ones exercised by a test.
  // This branch gets its own reason: an input reaching here has valid encoding
  // (it survived both checks above) and is simply not stable under this
  // normalizer, so reporting an encoding error would misdiagnose it.
  if (verifyFixedPoint) {
    const reNormalized = normalizeRepositoryUrlImpl(value, false);
    if (!reNormalized.ok || reNormalized.value !== value) {
      return {
        ok: false,
        reason: "The repository URL cannot be normalized to a stable canonical URL.",
      };
    }
  }

  return { ok: true, value };
}

function decodedPathSegments(url: URL): string[] | null {
  try {
    return url.pathname
      .split("/")
      .filter(Boolean)
      .map((segment) => decodeURIComponent(segment));
  } catch {
    return null;
  }
}

export function resolveShowcaseRepository(
  input: string,
  registry: readonly ShowcaseRegistryEntry[] = SHOWCASE_REGISTRY,
): ShowcaseLookup {
  const candidate = input.trim();
  for (const entry of registry) {
    if (!entry.aliases.includes(candidate)) continue;

    const canonical = normalizeRepositoryUrl(entry.canonicalUrl);
    if (canonical.ok) {
      return { status: "hit", normalizedUrl: canonical.value, entry };
    }
  }

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
