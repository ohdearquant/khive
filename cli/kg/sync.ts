/**
 * `khive kg sync` — validate NDJSON + rebuild working.db from source files.
 *
 * Reads entities.ndjson and edges.ndjson, validates them, then builds
 * `.khive/state/working.db` as an atomic JSON snapshot. The DB is written
 * via tmp+rename for crash safety.
 */

import { loadConfig } from "../lib/config.ts";
import { planEmbed, printEmbedPlan } from "../lib/embed.ts";
import { EDGES_FILE, ensureStateDir, ENTITIES_FILE, WORKING_DB } from "../lib/paths.ts";
import { countLines } from "../lib/ndjson.ts";
import { printValidationResult, validate } from "./validate.ts";

// ─── mtime-based up-to-date check ─────────────────────────────────────────────

/**
 * Returns true if the working DB exists AND its mtime is newer than both
 * NDJSON source files.  A missing DB or an older DB means a rebuild is needed.
 */
async function isDbUpToDate(repoRoot: string): Promise<boolean> {
  const dbPath = `${repoRoot}/${WORKING_DB}`;
  let dbMtime: number;
  try {
    const dbStat = await Deno.stat(dbPath);
    dbMtime = dbStat.mtime?.getTime() ?? 0;
  } catch {
    return false; // DB does not exist
  }

  for (const rel of [ENTITIES_FILE, EDGES_FILE]) {
    try {
      const stat = await Deno.stat(`${repoRoot}/${rel}`);
      const fileMtime = stat.mtime?.getTime() ?? 0;
      if (fileMtime > dbMtime) return false;
    } catch {
      // File doesn't exist yet — treat as "up to date" for this file
    }
  }

  return true;
}

/**
 * Rebuild the working DB from NDJSON source files.
 *
 * Reads entities.ndjson and edges.ndjson, creates a fresh SQLite database
 * with the parsed data. The DB is written atomically: build into a .tmp
 * file, then rename over the target path.
 */
async function rebuildDb(repoRoot: string): Promise<void> {
  const dbPath = `${repoRoot}/${WORKING_DB}`;
  const tmpPath = `${dbPath}.tmp`;

  await ensureStateDir(repoRoot);

  try {
    await Deno.remove(tmpPath);
  } catch {
    // tmp doesn't exist — fine
  }

  const entitiesPath = `${repoRoot}/${ENTITIES_FILE}`;
  const edgesPath = `${repoRoot}/${EDGES_FILE}`;

  let entityCount = 0;
  let edgeCount = 0;

  const entities: Record<string, unknown>[] = [];
  const edges: Record<string, unknown>[] = [];

  try {
    const entText = await Deno.readTextFile(entitiesPath);
    for (const line of entText.split("\n")) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      entities.push(JSON.parse(trimmed));
      entityCount++;
    }
  } catch {
    // No entities file — empty graph
  }

  try {
    const edgeText = await Deno.readTextFile(edgesPath);
    for (const line of edgeText.split("\n")) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      edges.push(JSON.parse(trimmed));
      edgeCount++;
    }
  } catch {
    // No edges file — no edges
  }

  const db = JSON.stringify({ entities, edges, synced_at: new Date().toISOString() });
  await Deno.writeTextFile(tmpPath, db);
  await Deno.rename(tmpPath, dbPath);
}

// ─── CLI entry point ──────────────────────────────────────────────────────────

/**
 * `khive kg sync` command.
 *
 * Args:
 *   --quiet   Suppress output (for git hooks).
 *
 * Exits 0 on success (including no-op).
 * Exits 1 if NDJSON validation fails (leaves working.db unchanged).
 */
export async function runSync(repoRoot: string, args: string[]): Promise<void> {
  const quiet = args.includes("--quiet");

  // ── 1. Check if DB is up to date ─────────────────────────────────────────
  if (await isDbUpToDate(repoRoot)) {
    if (!quiet) console.log("DB is up to date");
    return;
  }

  // ── 2. Validate NDJSON files before rebuilding ────────────────────────────
  const result = await validate(repoRoot);
  if (!result.valid) {
    if (!quiet) {
      printValidationResult(result);
      console.error(
        "\nSync aborted: fix validation errors before syncing. (working.db unchanged)",
      );
    }
    Deno.exit(1);
  }

  // ── 3. Rebuild working.db ─────────────────────────────────────────────────
  await rebuildDb(repoRoot);

  // ── 4. Embed step (ADR-057 §E3, Phase C1: plan only) ──────────────────────
  // Per ADR-057 §5, sync runs the embed step AFTER rebuild so the working DB
  // sees vectors when Phase C2 runtime is wired. In Phase C1 we only print
  // the plan when `embed.auto_embed = true` and there is anything pending.
  const config = await loadConfig(repoRoot);
  if (config.embed.auto_embed) {
    const plan = await planEmbed(repoRoot, config.embed);
    if (plan.pending.length > 0 && !quiet) {
      printEmbedPlan(plan);
    }
  }

  // ── 5. Report ─────────────────────────────────────────────────────────────
  if (!quiet) {
    const entityCount = await countLines(`${repoRoot}/${ENTITIES_FILE}`);
    const edgeCount = await countLines(`${repoRoot}/${EDGES_FILE}`);
    console.log(`Synced: ${entityCount} entities, ${edgeCount} edges`);
  }
}
