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

export async function loadStaticShowcaseBundle(
  entry: ShowcaseRegistryEntry,
  fetchBundle: ShowcaseFetch = fetch,
): Promise<RepoBundle> {
  if (!isAllowedShowcaseAsset(entry.assetPath)) {
    throw new Error("The curated registry referenced an unapproved showcase asset.");
  }

  const response = await fetchBundle(entry.assetPath, {
    cache: "force-cache",
    credentials: "same-origin",
    redirect: "error",
  });
  if (!response.ok) {
    throw new Error(`Showcase asset could not be loaded (HTTP ${response.status}).`);
  }

  const declaredLength = Number(response.headers.get("content-length"));
  if (Number.isFinite(declaredLength) && declaredLength > REPO_BUNDLE_MAX_BYTES) {
    throw new Error(`Showcase bundle exceeds the ${REPO_BUNDLE_MAX_MIB} MiB browser limit.`);
  }

  const bytes = new Uint8Array(await response.arrayBuffer());
  if (bytes.byteLength > REPO_BUNDLE_MAX_BYTES) {
    throw new Error(`Showcase bundle exceeds the ${REPO_BUNDLE_MAX_MIB} MiB browser limit.`);
  }

  let value: unknown;
  try {
    value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
  } catch {
    throw new Error("Showcase asset is not valid JSON.");
  }
  return parseRepoBundle(value);
}
