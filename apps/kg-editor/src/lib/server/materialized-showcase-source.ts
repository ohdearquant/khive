import { createHash } from "node:crypto";
import { constants } from "node:fs";
import { type FileHandle, lstat, open, realpath } from "node:fs/promises";
import { isAbsolute, relative, resolve } from "node:path";

import { parseRepoBundle, type RepoBundle } from "@/lib/repo-bundle";

export const SHOWCASE_ANALYSIS_MAX_BYTES = 8 * 1024 * 1024;
const ANALYSIS_REPORT_NAME = "khive.repo.v1.json";
const analysisIdPattern = /^[a-z0-9](?:[a-z0-9-]{0,62}[a-z0-9])?$/;

export type ShowcaseAnalysisErrorCode =
  | "NOT_CONFIGURED"
  | "ANALYSIS_BUILDING"
  | "ANALYSIS_INVALID"
  | "ANALYSIS_TOO_LARGE"
  | "ANALYSIS_UNAVAILABLE";

const publicMessages: Record<ShowcaseAnalysisErrorCode, string> = {
  NOT_CONFIGURED: "This repository analysis is not configured.",
  ANALYSIS_BUILDING: "This repository analysis is still being prepared.",
  ANALYSIS_INVALID: "This repository analysis did not pass validation.",
  ANALYSIS_TOO_LARGE:
    "This repository analysis exceeds the browser delivery limit.",
  ANALYSIS_UNAVAILABLE: "This repository analysis is temporarily unavailable.",
};

const statusCodes: Record<ShowcaseAnalysisErrorCode, number> = {
  NOT_CONFIGURED: 404,
  ANALYSIS_BUILDING: 503,
  ANALYSIS_INVALID: 500,
  ANALYSIS_TOO_LARGE: 413,
  ANALYSIS_UNAVAILABLE: 503,
};

export class ShowcaseAnalysisError extends Error {
  readonly code: ShowcaseAnalysisErrorCode;
  readonly status: number;

  constructor(code: ShowcaseAnalysisErrorCode) {
    super(publicMessages[code]);
    this.name = "ShowcaseAnalysisError";
    this.code = code;
    this.status = statusCodes[code];
  }
}

export type ShowcaseAnalysisRegistry = Readonly<{
  root: string;
  ids: ReadonlySet<string>;
}>;

export type MaterializedShowcaseAnalysis = Readonly<{
  bytes: Uint8Array<ArrayBuffer>;
  bundle: RepoBundle;
  etag: string;
}>;

type Environment = Readonly<Record<string, string | undefined>>;

function configuredAnalysisIds(value: string | undefined): ReadonlySet<string> {
  const ids = new Set(
    (value ?? "")
      .split(",")
      .map((id) => id.trim())
      .filter(Boolean),
  );
  if (ids.size === 0 || [...ids].some((id) => !analysisIdPattern.test(id))) {
    throw new ShowcaseAnalysisError("NOT_CONFIGURED");
  }
  return ids;
}

export function resolveShowcaseAnalysisRegistry(
  environment: Environment = process.env,
): ShowcaseAnalysisRegistry {
  const root = environment.KHIVE_SHOWCASE_ANALYSIS_ROOT?.trim();
  if (!root || !isAbsolute(root)) {
    throw new ShowcaseAnalysisError("NOT_CONFIGURED");
  }
  return {
    root,
    ids: configuredAnalysisIds(environment.KHIVE_SHOWCASE_ANALYSIS_IDS),
  };
}

function isContainedPath(root: string, candidate: string): boolean {
  const suffix = relative(root, candidate);
  return suffix !== "" && !suffix.startsWith("..") && !isAbsolute(suffix);
}

async function checkedDirectory(path: string): Promise<void> {
  const metadata = await lstat(path);
  if (!metadata.isDirectory() || metadata.isSymbolicLink()) {
    throw new ShowcaseAnalysisError("ANALYSIS_INVALID");
  }
}

async function verifyOpenedReport(
  openedMetadata: Awaited<ReturnType<FileHandle["stat"]>>,
  root: string,
  canonicalRoot: string,
  analysisDirectory: string,
  reportPath: string,
): Promise<void> {
  await checkedDirectory(root);
  if (await realpath(root) !== canonicalRoot) {
    throw new ShowcaseAnalysisError("ANALYSIS_INVALID");
  }
  await checkedDirectory(analysisDirectory);

  const pathMetadata = await lstat(reportPath);
  if (
    !pathMetadata.isFile() || pathMetadata.isSymbolicLink() ||
    pathMetadata.dev !== openedMetadata.dev ||
    pathMetadata.ino !== openedMetadata.ino
  ) {
    throw new ShowcaseAnalysisError("ANALYSIS_INVALID");
  }
  const reopenedCanonicalReport = await realpath(reportPath);
  if (!isContainedPath(canonicalRoot, reopenedCanonicalReport)) {
    throw new ShowcaseAnalysisError("ANALYSIS_INVALID");
  }
}

async function readBoundedReport(
  handle: FileHandle,
): Promise<Uint8Array<ArrayBuffer>> {
  const buffer = Buffer.allocUnsafe(SHOWCASE_ANALYSIS_MAX_BYTES + 1);
  let offset = 0;
  while (offset < buffer.byteLength) {
    const { bytesRead } = await handle.read(
      buffer,
      offset,
      buffer.byteLength - offset,
      offset,
    );
    if (bytesRead === 0) break;
    offset += bytesRead;
  }
  if (offset > SHOWCASE_ANALYSIS_MAX_BYTES) {
    throw new ShowcaseAnalysisError("ANALYSIS_TOO_LARGE");
  }
  return Uint8Array.from(buffer.subarray(0, offset));
}

function mapFilesystemError(error: unknown): ShowcaseAnalysisError {
  if (error instanceof ShowcaseAnalysisError) return error;
  if ((error as NodeJS.ErrnoException | undefined)?.code === "ENOENT") {
    return new ShowcaseAnalysisError("ANALYSIS_BUILDING");
  }
  return new ShowcaseAnalysisError("ANALYSIS_UNAVAILABLE");
}

export async function loadMaterializedShowcaseAnalysis(
  id: string,
  registry: ShowcaseAnalysisRegistry = resolveShowcaseAnalysisRegistry(),
): Promise<MaterializedShowcaseAnalysis> {
  if (!analysisIdPattern.test(id) || !registry.ids.has(id)) {
    throw new ShowcaseAnalysisError("NOT_CONFIGURED");
  }

  let bytes: Uint8Array<ArrayBuffer>;
  try {
    await checkedDirectory(registry.root);
    const canonicalRoot = await realpath(registry.root);
    const analysisDirectory = resolve(registry.root, id);
    await checkedDirectory(analysisDirectory);
    const reportPath = resolve(analysisDirectory, ANALYSIS_REPORT_NAME);
    const reportMetadata = await lstat(reportPath);
    if (!reportMetadata.isFile() || reportMetadata.isSymbolicLink()) {
      throw new ShowcaseAnalysisError("ANALYSIS_INVALID");
    }
    if (reportMetadata.size > SHOWCASE_ANALYSIS_MAX_BYTES) {
      throw new ShowcaseAnalysisError("ANALYSIS_TOO_LARGE");
    }

    const canonicalReport = await realpath(reportPath);
    if (!isContainedPath(canonicalRoot, canonicalReport)) {
      throw new ShowcaseAnalysisError("ANALYSIS_INVALID");
    }

    const noFollow = typeof constants.O_NOFOLLOW === "number"
      ? constants.O_NOFOLLOW
      : 0;
    const handle = await open(reportPath, constants.O_RDONLY | noFollow);
    try {
      const openedMetadata = await handle.stat();
      if (!openedMetadata.isFile()) {
        throw new ShowcaseAnalysisError("ANALYSIS_INVALID");
      }
      if (openedMetadata.size > SHOWCASE_ANALYSIS_MAX_BYTES) {
        throw new ShowcaseAnalysisError("ANALYSIS_TOO_LARGE");
      }
      await verifyOpenedReport(
        openedMetadata,
        registry.root,
        canonicalRoot,
        analysisDirectory,
        reportPath,
      );
      bytes = await readBoundedReport(handle);
    } finally {
      await handle.close();
    }
  } catch (error) {
    throw mapFilesystemError(error);
  }

  let bundle: RepoBundle;
  try {
    const json = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    bundle = parseRepoBundle(JSON.parse(json));
  } catch {
    throw new ShowcaseAnalysisError("ANALYSIS_INVALID");
  }

  const digest = createHash("sha256").update(bytes).digest("hex");
  return {
    bytes,
    bundle,
    etag: `"sha256-${digest}"`,
  };
}

export function showcaseAnalysisErrorBody(error: ShowcaseAnalysisError) {
  return {
    error: {
      code: error.code,
      message: error.message,
    },
  } as const;
}
