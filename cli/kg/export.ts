/**
 * `khive kg export` — write canonical NDJSON files (default) or a KgArchive
 * JSON bundle (--format archive).
 *
 * Default behavior (ADR-048 §4):
 *   Reads .khive/kg/entities.ndjson and .khive/kg/edges.ndjson, re-serializes
 *   each record in canonical field order, and writes the result back to the
 *   same files.  Running export twice with no intervening writes produces
 *   bit-identical files (idempotent).
 *
 * Archive mode (--format archive):
 *   Assembles a KgArchive JSON bundle and writes it to --output <file> or
 *   stdout.  This output is non-canonical (it may include metadata such as the
 *   archive format envelope) and is not suitable for git-tracked history.
 */

import { EDGES_FILE, ENTITIES_FILE, KG_DIR } from "../lib/paths.ts";
import { parseEdgeLine, parseEntityLine, readNdjson } from "../lib/ndjson.ts";
import { canonicalEdgeJson, canonicalEntityJson } from "../lib/canonical.ts";
import { exec } from "../lib/git.ts";
import { KG_ARCHIVE_FORMAT, KG_ARCHIVE_VERSION } from "../version.ts";

// ─── Default export: canonical NDJSON files ──────────────────────────────────

/**
 * Re-write .khive/kg/entities.ndjson and .khive/kg/edges.ndjson in canonical
 * ADR-048 field order.
 *
 * If the files do not exist they are treated as empty and created.
 * Existing files are overwritten in place (this is an explicit export
 * operation, not a silent mutation).
 *
 * Throws with a descriptive message on parse errors.
 * Reports entity/edge counts to stderr.
 */
export async function exportCanonical(repoRoot: string): Promise<void> {
  // ── 1. Read entities.ndjson ───────────────────────────────────────────────
  const entitiesPath = `${repoRoot}/${ENTITIES_FILE}`;
  const entityRecords: Record<string, unknown>[] = [];

  for await (const entry of readNdjson(entitiesPath)) {
    if (entry.data === null) {
      throw new Error(`Error reading entities.ndjson: ${entry.error}`);
    }
    const entity = parseEntityLine(entry.data);
    if (!entity) {
      throw new Error(`invalid entity on line ${entry.line} of ${ENTITIES_FILE}`);
    }
    entityRecords.push(entry.data);
  }

  // ── 2. Read edges.ndjson ──────────────────────────────────────────────────
  const edgesPath = `${repoRoot}/${EDGES_FILE}`;
  const edgeRecords: Record<string, unknown>[] = [];

  for await (const entry of readNdjson(edgesPath)) {
    if (entry.data === null) {
      throw new Error(`Error reading edges.ndjson: ${entry.error}`);
    }
    const edge = parseEdgeLine(entry.data);
    if (!edge) {
      throw new Error(`invalid edge on line ${entry.line} of ${EDGES_FILE}`);
    }
    edgeRecords.push(entry.data);
  }

  // ── 3. Sort records per ADR-048 §4 then serialize in canonical field order ──
  //
  // ADR-048:110-113 + 240-244 require:
  //   entities sorted by UUID (lowercased) ascending
  //   edges sorted by composite key (source + target + relation) ascending
  entityRecords.sort((a, b) =>
    (a["id"] as string).toLowerCase().localeCompare((b["id"] as string).toLowerCase())
  );

  edgeRecords.sort((a, b) => {
    const aKey = `${(a["source"] as string).toLowerCase()}\x00${
      (a["target"] as string).toLowerCase()
    }\x00${(a["relation"] as string).toLowerCase()}`;
    const bKey = `${(b["source"] as string).toLowerCase()}\x00${
      (b["target"] as string).toLowerCase()
    }\x00${(b["relation"] as string).toLowerCase()}`;
    return aKey.localeCompare(bKey);
  });

  const entitiesNdjson = entityRecords.map((e) => canonicalEntityJson(e)).join("\n") +
    (entityRecords.length > 0 ? "\n" : "");

  const edgesNdjson = edgeRecords.map((e) => canonicalEdgeJson(e)).join("\n") +
    (edgeRecords.length > 0 ? "\n" : "");

  // ── 4. Write canonical files ──────────────────────────────────────────────
  await Deno.mkdir(`${repoRoot}/${KG_DIR}`, { recursive: true });
  await Deno.writeTextFile(entitiesPath, entitiesNdjson);
  await Deno.writeTextFile(edgesPath, edgesNdjson);

  // ── 5. Report ─────────────────────────────────────────────────────────────
  console.error(
    `Exported ${entityRecords.length} entities and ${edgeRecords.length} edges`,
  );
}

// ─── Archive export: single JSON bundle (--format archive) ───────────────────

interface ExportArchiveOptions {
  outputPath?: string;
  namespace?: string;
}

/**
 * Export NDJSON files from repoRoot as a KgArchive JSON bundle.
 *
 * This is the legacy / compatibility mode: it produces a single JSON object
 * with a format envelope.  The output is NOT idempotent (it includes an
 * `exported_at` timestamp) and is NOT the canonical git-native format.
 *
 * Accepts a plain string outputPath (legacy) or an ExportArchiveOptions object.
 * Writes to outputPath if provided, otherwise to stdout.
 * Reports entity/edge counts to stderr.
 */
export async function exportArchive(
  repoRoot: string,
  options: ExportArchiveOptions | string = {},
): Promise<void> {
  const outputPath = typeof options === "string" ? options : options.outputPath;
  const namespace = typeof options === "string" ? "local" : (options.namespace ?? "local");
  // ── 1. Read entities.ndjson ───────────────────────────────────────────────
  const entitiesPath = `${repoRoot}/${ENTITIES_FILE}`;
  const entities: Record<string, unknown>[] = [];

  for await (const entry of readNdjson(entitiesPath)) {
    if (entry.data === null) {
      throw new Error(`Error reading entities.ndjson: ${entry.error}`);
    }
    const entity = parseEntityLine(entry.data);
    if (!entity) {
      throw new Error(`invalid entity on line ${entry.line} of ${ENTITIES_FILE}`);
    }
    entities.push(entry.data);
  }

  // ── 2. Read edges.ndjson ──────────────────────────────────────────────────
  const edgesPath = `${repoRoot}/${EDGES_FILE}`;
  const edges: Record<string, unknown>[] = [];

  for await (const entry of readNdjson(edgesPath)) {
    if (entry.data === null) {
      throw new Error(`Error reading edges.ndjson: ${entry.error}`);
    }
    const edge = parseEdgeLine(entry.data);
    if (!edge) {
      throw new Error(`invalid edge on line ${entry.line} of ${EDGES_FILE}`);
    }
    edges.push(entry.data);
  }

  // ── 3. Build KgArchive ────────────────────────────────────────────────────
  const archive = {
    format: KG_ARCHIVE_FORMAT,
    version: KG_ARCHIVE_VERSION,
    namespace,
    exported_at: new Date().toISOString(),
    entities,
    edges,
  };

  const output = JSON.stringify(archive, null, 2);

  // ── 4. Write to file or stdout ────────────────────────────────────────────
  if (outputPath) {
    await Deno.writeTextFile(outputPath, output + "\n");
  } else {
    console.log(output);
  }

  // ── 5. Report to stderr ───────────────────────────────────────────────────
  console.error(`Exported ${entities.length} entities and ${edges.length} edges`);
}

// ─── Namespace resolution (ADR-051 §2) ───────────────────────────────────────

const NAMESPACE_RE = /^[a-z0-9][a-z0-9_-]{0,62}[a-z0-9]$/;

async function resolveExportNamespace(
  repoRoot: string,
  flagValue?: string,
): Promise<string> {
  let candidate: string | undefined;

  if (flagValue) {
    candidate = flagValue;
  } else {
    try {
      const settingsPath = `${repoRoot}/.khive/settings.json`;
      const text = await Deno.readTextFile(settingsPath);
      const settings = JSON.parse(text) as Record<string, unknown>;
      const actor = settings["actor"] as Record<string, unknown> | undefined;
      if (typeof actor?.["name"] === "string" && actor["name"].length > 0) {
        candidate = actor["name"] as string;
      }
    } catch {
      // Not found or invalid JSON — continue
    }
  }

  if (!candidate) {
    try {
      const result = await exec(["git", "remote", "get-url", "origin"]);
      if (result.code === 0 && result.stdout) {
        const url = result.stdout.trim();
        const match = url.match(/[:/]([^/]+?)(?:\.git)?$/);
        if (match) candidate = match[1];
      }
    } catch {
      // No remote — continue
    }
  }

  if (!candidate) {
    const parts = repoRoot.split("/");
    candidate = parts[parts.length - 1] ?? "local";
  }

  if (!NAMESPACE_RE.test(candidate)) {
    candidate = "local";
  }

  return candidate;
}

// ─── CLI entry point ──────────────────────────────────────────────────────────

function valueAfter(args: string[], flag: string): string | undefined {
  const idx = args.indexOf(flag);
  return idx !== -1 ? args[idx + 1] : undefined;
}

/**
 * `khive kg export [--format archive] [--output <file>] [--namespace <ns>]` command.
 *
 * Default: re-writes .khive/kg/entities.ndjson and .khive/kg/edges.ndjson
 *   in canonical ADR-048 field order.  Idempotent.
 *
 * --format archive [--output <file>] [--namespace <ns>]:
 *   Produces a single KgArchive JSON bundle.  Writes to <file> or stdout.
 *   Namespace defaults via ADR-051 resolution when --namespace is absent.
 *   Non-canonical; includes exported_at timestamp (not idempotent).
 *
 * Exits 0 on success, 1 on error.
 */
export async function runExport(repoRoot: string, args: string[]): Promise<void> {
  const format = valueAfter(args, "--format") ?? "ndjson";

  if (format === "archive") {
    const outputPath = valueAfter(args, "--output");
    const namespaceFlag = valueAfter(args, "--namespace");
    if (args.includes("--output") && !outputPath) {
      console.error("Error: --output requires a file path argument");
      Deno.exit(1);
    }
    if (args.includes("--namespace") && !namespaceFlag) {
      console.error("Error: --namespace requires a namespace argument");
      Deno.exit(1);
    }

    const namespace = await resolveExportNamespace(repoRoot, namespaceFlag);
    try {
      await exportArchive(repoRoot, { outputPath, namespace });
    } catch (err) {
      console.error(`Error: ${(err as Error).message}`);
      Deno.exit(1);
    }
    return;
  }

  if (format !== "ndjson") {
    // ADR-036 §8: the --format flag on export is reserved; any non-ndjson value
    // is rejected with a "not yet implemented" error until P1/P2 adapters ship.
    console.error(
      `Error: --format ${JSON.stringify(format)} is not yet implemented for export.\n` +
        `Supported: ndjson (default), archive.\n` +
        `Non-NDJSON export formats are deferred to P1/P2 (ADR-036 §8).`,
    );
    Deno.exit(1);
  }

  // Default: canonical NDJSON export
  try {
    await exportCanonical(repoRoot);
  } catch (err) {
    console.error(`Error: ${(err as Error).message}`);
    Deno.exit(1);
  }
}
