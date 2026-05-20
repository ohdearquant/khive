/**
 * `khive kg sync` — validate NDJSON + create working.db placeholder (Phase C1).
 *
 * Phase C1 scope: validate the NDJSON files and touch `.khive/state/working.db` to record
 * the sync timestamp. Full DB rebuild from NDJSON (the atomic import defined in ADR-052 §5)
 * is Phase C2 and is not yet integrated — that requires the Rust runtime to be available.
 *
 * The actual DB path is `.khive/state/working.db` (gitignored).
 */

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
 * "Rebuild" the working DB.
 *
 * Phase C1 stub: touch `.khive/state/working.db` to record the sync time.
 * When the Rust runtime is integrated, this will run the atomic import.
 */
async function rebuildDb(repoRoot: string): Promise<void> {
  const dbPath = `${repoRoot}/${WORKING_DB}`;

  // Ensure .khive/state/ exists (works even without running `khive kg init`)
  await ensureStateDir(repoRoot);

  // Touch the DB file (Phase C1: represents a completed sync)
  try {
    const existing = await Deno.stat(dbPath);
    if (existing) {
      // Update mtime by opening the file for writing (no-op content change)
      const f = await Deno.open(dbPath, { write: true, create: true });
      f.close();
    }
  } catch {
    // Create a new empty file
    await Deno.writeTextFile(dbPath, "");
  }
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

  // ── 4. Report ─────────────────────────────────────────────────────────────
  if (!quiet) {
    const entityCount = await countLines(`${repoRoot}/${ENTITIES_FILE}`);
    const edgeCount = await countLines(`${repoRoot}/${EDGES_FILE}`);
    console.log(`Synced: ${entityCount} entities, ${edgeCount} edges`);
  }
}
