import { parseRepoBundle, type RepoBundle } from "@/lib/repo-bundle";
import {
  isAllowedShowcaseAsset,
  type ShowcaseRegistryEntry,
} from "@/lib/showcase-registry";

export const REPO_BUNDLE_MAX_MIB = 8;
export const REPO_BUNDLE_MAX_BYTES = REPO_BUNDLE_MAX_MIB * 1024 * 1024;

export type ShowcaseFetch = (
  input: string,
  init?: RequestInit,
) => Promise<Pick<Response, "ok" | "status" | "headers" | "arrayBuffer">>;

export type ShowcaseResponse = Awaited<ReturnType<ShowcaseFetch>>;

export async function parseBoundedShowcaseResponse(
  response: ShowcaseResponse,
  sourceLabel = "Showcase bundle",
): Promise<RepoBundle> {
  const declaredLength = Number(response.headers.get("content-length"));
  if (Number.isFinite(declaredLength) && declaredLength > REPO_BUNDLE_MAX_BYTES) {
    throw new Error(`${sourceLabel} exceeds the ${REPO_BUNDLE_MAX_MIB} MiB browser limit.`);
  }

  const bytes = new Uint8Array(await response.arrayBuffer());
  if (bytes.byteLength > REPO_BUNDLE_MAX_BYTES) {
    throw new Error(`${sourceLabel} exceeds the ${REPO_BUNDLE_MAX_MIB} MiB browser limit.`);
  }

  let value: unknown;
  try {
    value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
  } catch {
    throw new Error(`${sourceLabel} is not valid JSON.`);
  }
  return parseRepoBundle(value);
}

export async function loadStaticShowcaseBundle(
  entry: ShowcaseRegistryEntry,
  fetchBundle: ShowcaseFetch = fetch,
): Promise<RepoBundle> {
  const assetPath = entry.assetPath;
  if (!isAllowedShowcaseAsset(assetPath)) {
    throw new Error("The curated registry referenced an unapproved showcase asset.");
  }

  const response = await fetchBundle(assetPath, {
    cache: "force-cache",
    credentials: "same-origin",
    redirect: "error",
  });
  if (!response.ok) {
    throw new Error(`Showcase asset could not be loaded (HTTP ${response.status}).`);
  }

  return parseBoundedShowcaseResponse(response);
}
