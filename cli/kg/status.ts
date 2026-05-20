/**
 * `khive kg status` — summarize KG state against last git commit (ADR-051 §3).
 *
 * Shows entity/edge counts, modified-since-last-commit counts, and validation state.
 * No git writes occur.
 */

import { exec } from "../lib/git.ts";
import {
  EDGES_FILE,
  ensureStateDir,
  ENTITIES_FILE,
  KG_DIR,
  SCHEMA_FILE as _SCHEMA_FILE,
} from "../lib/paths.ts";
import { countLines } from "../lib/ndjson.ts";
import { loadSchema } from "../lib/schema.ts";
import { validate } from "./validate.ts";

// ─── Git diff helpers ─────────────────────────────────────────────────────────

/**
 * Returns the names of .khive/kg/ files that differ from HEAD.
 * Returns an empty array if the repo has no commits yet.
 */
async function getModifiedKgFiles(repoRoot: string): Promise<string[]> {
  const result = await exec([
    "git",
    "diff",
    "--name-only",
    "HEAD",
    "--",
    `${repoRoot}/${KG_DIR}`,
  ]);
  if (result.code !== 0) {
    // No commits yet — everything is untracked
    return [];
  }
  return result.stdout
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l.length > 0);
}

/**
 * Reads the committed (HEAD) content of a file using `git show HEAD:<path>`.
 * The path must be relative to the repo root.
 * Returns null if the file is not tracked in HEAD.
 */
async function getHeadContent(
  _repoRoot: string,
  relativePath: string,
): Promise<string | null> {
  const result = await exec([
    "git",
    "show",
    `HEAD:${relativePath}`,
  ]);
  if (result.code !== 0) return null;
  return result.stdout;
}

/**
 * Count data lines (non-blank, non-comment) in an NDJSON string.
 */
function countNdjsonLines(content: string): number {
  let count = 0;
  for (const line of content.split("\n")) {
    const raw = line.trim();
    if (raw !== "" && !raw.startsWith("#")) count++;
  }
  return count;
}

// ─── Status computation ───────────────────────────────────────────────────────

interface KgStatus {
  namespace: string;
  schemaValid: boolean;
  entityKindCount: number;
  edgeRelationCount: number;
  remoteCount: number;
  entityCount: number;
  entityChangedCount: number;
  edgeCount: number;
  edgeChangedCount: number;
  modifiedFiles: Array<{ path: string; description: string }>;
  validationErrors: number;
  validationErrorMessages: string[];
}

async function computeStatus(
  repoRoot: string,
  namespace: string,
): Promise<KgStatus> {
  // ── Schema ────────────────────────────────────────────────────────────────
  let schemaValid = false;
  let entityKindCount = 0;
  let edgeRelationCount = 0;
  let remoteCount = 0;

  try {
    const schema = await loadSchema(repoRoot);
    entityKindCount = schema.entity_kinds.length;
    edgeRelationCount = schema.edge_relations.length;
    remoteCount = schema.remotes ? Object.keys(schema.remotes).length : 0;
    schemaValid = entityKindCount > 0 && edgeRelationCount > 0;
  } catch {
    schemaValid = false;
  }

  // ── Current counts ────────────────────────────────────────────────────────
  const entityCount = await countLines(`${repoRoot}/${ENTITIES_FILE}`);
  const edgeCount = await countLines(`${repoRoot}/${EDGES_FILE}`);

  // ── Modified files + change counts ───────────────────────────────────────
  const modifiedPaths = await getModifiedKgFiles(repoRoot);
  const modifiedFiles: Array<{ path: string; description: string }> = [];
  let entityChangedCount = 0;
  let edgeChangedCount = 0;

  for (const filePath of modifiedPaths) {
    const relative = filePath; // already relative to repo root from git
    const headContent = await getHeadContent(repoRoot, relative);

    if (relative.endsWith("entities.ndjson")) {
      const headCount = headContent ? countNdjsonLines(headContent) : 0;
      const diff = Math.abs(entityCount - headCount);
      entityChangedCount = diff;
      modifiedFiles.push({
        path: relative,
        description: `${diff} entit${diff === 1 ? "y" : "ies"} changed`,
      });
    } else if (relative.endsWith("edges.ndjson")) {
      const headCount = headContent ? countNdjsonLines(headContent) : 0;
      const diff = Math.abs(edgeCount - headCount);
      edgeChangedCount = diff;
      modifiedFiles.push({
        path: relative,
        description: `${diff} edge${diff === 1 ? "" : "s"} ${
          edgeCount >= headCount ? "added" : "removed"
        }`,
      });
    } else {
      modifiedFiles.push({ path: relative, description: "modified" });
    }
  }

  // ── Validation ────────────────────────────────────────────────────────────
  const validationResult = await validate(repoRoot);

  return {
    namespace,
    schemaValid,
    entityKindCount,
    edgeRelationCount,
    remoteCount,
    entityCount,
    entityChangedCount,
    edgeCount,
    edgeChangedCount,
    modifiedFiles,
    validationErrors: validationResult.errors.length,
    validationErrorMessages: validationResult.errors
      .slice(0, 5)
      .map((e) => `${e.file}:${e.line}: ${e.message}`),
  };
}

// ─── Output formatting ────────────────────────────────────────────────────────

function formatStatus(s: KgStatus): string {
  const lines: string[] = [];

  lines.push(`KG Status (namespace: ${s.namespace})`);

  // Schema line
  const schemaStatus = s.schemaValid ? "valid" : "invalid";
  lines.push(
    `  Schema: ${schemaStatus} (${s.entityKindCount} kinds, ${s.edgeRelationCount} relations, ${s.remoteCount} remote${
      s.remoteCount === 1 ? "" : "s"
    })`,
  );

  // Entity line
  const entityDiff = s.entityChangedCount > 0
    ? ` (${s.entityChangedCount} modified since last commit)`
    : s.modifiedFiles.some((f) => f.path.endsWith("entities.ndjson"))
    ? " (modified since last commit)"
    : "";
  lines.push(`  Entities: ${s.entityCount}${entityDiff}`);

  // Edge line
  const edgeDiff = s.edgeChangedCount > 0
    ? ` (${s.edgeChangedCount} new since last commit)`
    : s.modifiedFiles.some((f) => f.path.endsWith("edges.ndjson"))
    ? " (modified since last commit)"
    : "";
  lines.push(`  Edges: ${s.edgeCount}${edgeDiff}`);

  // Modified files
  if (s.modifiedFiles.length > 0) {
    lines.push("");
    lines.push("  Modified files:");
    for (const f of s.modifiedFiles) {
      lines.push(`    M ${f.path} (${f.description})`);
    }
  }

  // Validation
  lines.push("");
  if (s.validationErrors === 0) {
    lines.push("  Validation: pass");
  } else {
    lines.push(
      `  Validation: fail — ${s.validationErrors} error${s.validationErrors === 1 ? "" : "s"}`,
    );
    for (const msg of s.validationErrorMessages) {
      lines.push(`    ${msg}`);
    }
    if (s.validationErrors > 5) {
      lines.push(`    ... and ${s.validationErrors - 5} more`);
    }
  }

  return lines.join("\n");
}

// ─── Namespace resolution (ADR-051 §2) ───────────────────────────────────────

/**
 * Resolve the KG namespace for the current repo.
 * Order: --namespace flag → .khive/settings.json actor.name → git remote → dir name.
 */
async function resolveNamespace(
  repoRoot: string,
  flagValue?: string,
): Promise<string> {
  if (flagValue) return flagValue;

  // Try .khive/settings.json
  try {
    const settingsPath = `${repoRoot}/.khive/settings.json`;
    const text = await Deno.readTextFile(settingsPath);
    const settings = JSON.parse(text) as Record<string, unknown>;
    const actor = settings["actor"] as Record<string, unknown> | undefined;
    if (typeof actor?.["name"] === "string" && actor["name"].length > 0) {
      return actor["name"] as string;
    }
  } catch {
    // Not found or invalid JSON — continue
  }

  // Try git remote URL
  try {
    const { exec: gitExec } = await import("../lib/git.ts");
    const result = await gitExec(["git", "remote", "get-url", "origin"]);
    if (result.code === 0 && result.stdout) {
      const url = result.stdout.trim();
      // Extract repo name from ssh: git@github.com:owner/repo.git or https: .../repo.git
      const match = url.match(/[:/]([^/]+?)(?:\.git)?$/);
      if (match) return match[1];
    }
  } catch {
    // No remote — continue
  }

  // Fall back to directory name
  const parts = repoRoot.split("/");
  return parts[parts.length - 1] ?? "unknown";
}

// ─── CLI entry point ──────────────────────────────────────────────────────────

/**
 * `khive kg status` command.
 *
 * Args:
 *   --namespace <ns>   Override namespace detection.
 *
 * Exits 0 always (status is informational).
 */
export async function runStatus(repoRoot: string, args: string[]): Promise<void> {
  // Check KG initialized
  try {
    await Deno.stat(`${repoRoot}/${KG_DIR}`);
  } catch {
    console.log("KG not initialized. Run 'khive kg init' to start.");
    return;
  }

  // Ensure .khive/state/ exists — works after git clone without init
  await ensureStateDir(repoRoot);

  // Parse --namespace flag
  let namespace: string | undefined;
  const nsIdx = args.indexOf("--namespace");
  if (nsIdx !== -1 && args[nsIdx + 1]) {
    namespace = args[nsIdx + 1];
  }

  const resolvedNs = await resolveNamespace(repoRoot, namespace);
  const status = await computeStatus(repoRoot, resolvedNs);
  console.log(formatStatus(status));
}
