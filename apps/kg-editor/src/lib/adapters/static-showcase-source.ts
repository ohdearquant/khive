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
) => Promise<
  & Pick<Response, "ok" | "status" | "headers" | "arrayBuffer">
  & { body?: Response["body"] }
>;

export type ShowcaseResponse = Awaited<ReturnType<ShowcaseFetch>>;

function oversizeError(sourceLabel: string): Error {
  return new Error(
    `${sourceLabel} exceeds the ${REPO_BUNDLE_MAX_MIB} MiB browser limit.`,
  );
}

// Reads the body incrementally and aborts the moment the byte budget is
// exceeded, so a chunked or missing-Content-Length response can never be
// fully materialized before rejection. Falls back to a single bounded
// arrayBuffer() read only when no stream reader is available.
async function readBoundedBytes(
  response: ShowcaseResponse,
  sourceLabel: string,
): Promise<Uint8Array> {
  const body = response.body;
  if (!body) {
    const bytes = new Uint8Array(await response.arrayBuffer());
    if (bytes.byteLength > REPO_BUNDLE_MAX_BYTES) {
      throw oversizeError(sourceLabel);
    }
    return bytes;
  }

  const reader = body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > REPO_BUNDLE_MAX_BYTES) {
      await reader.cancel().catch(() => {});
      throw oversizeError(sourceLabel);
    }
    chunks.push(value);
  }

  const merged = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    merged.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return merged;
}

export async function parseBoundedShowcaseResponse(
  response: ShowcaseResponse,
  sourceLabel = "Showcase bundle",
): Promise<RepoBundle> {
  const declaredLength = Number(response.headers.get("content-length"));
  if (Number.isFinite(declaredLength) && declaredLength > REPO_BUNDLE_MAX_BYTES) {
    throw oversizeError(sourceLabel);
  }

  const bytes = await readBoundedBytes(response, sourceLabel);

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

  return parseBoundedShowcaseResponse(response);
}
