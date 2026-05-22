/**
 * `khive kg sync` — validate NDJSON, then build a real SQLite working DB by
 * shelling out to the Rust kernel (`kkernel sync`, see ADR-076).
 *
 * The Deno CLI orchestrates user-facing flow (validation, embed planning,
 * reporting). The kernel does the SQLite + FTS5 + vector work because that
 * layer lives in Rust crates (khive-runtime / khive-db).
 *
 * The working DB at `.khive/state/working.db` is a real SQLite file usable by
 * any consumer (khive-mcp, `sqlite3` CLI, downstream tools). This replaces
 * the misleading JSON-as-DB placeholder fixed in issue #174.
 */

import { loadConfig } from "../lib/config.ts";
import { planEmbed, printEmbedPlan } from "../lib/embed.ts";
import { EDGES_FILE, ensureStateDir, ENTITIES_FILE, WORKING_DB } from "../lib/paths.ts";
import { countLines } from "../lib/ndjson.ts";
import { printValidationResult, validate } from "./validate.ts";
import { runKernelSync } from "../lib/kernel.ts";

// ─── mtime-based up-to-date check ─────────────────────────────────────────────

/**
 * Returns true if the working DB exists AND its mtime is newer than both
 * NDJSON source files. A missing DB or an older DB means a rebuild is needed.
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

// ─── CLI entry point ──────────────────────────────────────────────────────────

/**
 * `khive kg sync` command.
 *
 * Args:
 *   --quiet   Suppress output (for git hooks).
 *
 * Exits 0 on success (including no-op).
 * Exits 1 if NDJSON validation fails (leaves working.db unchanged).
 * Exits 1 if kkernel sync fails (leaves working.db unchanged; tmp file left
 *   behind for post-mortem).
 */
export async function runSync(repoRoot: string, args: string[]): Promise<void> {
  const quiet = args.includes("--quiet");
  const dbPath = `${repoRoot}/${WORKING_DB}`;

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

  // ── 3. Build the SQLite DB via kkernel ────────────────────────────────────
  await ensureStateDir(repoRoot);
  let report;
  try {
    report = await runKernelSync(repoRoot, dbPath, "local");
  } catch (err) {
    if (!quiet) {
      console.error(`\n${(err as Error).message}`);
    }
    Deno.exit(1);
  }

  // ── 4. Embed step (ADR-057 §E3, Phase C1: plan only) ──────────────────────
  const config = await loadConfig(repoRoot);
  if (config.embed.auto_embed) {
    const plan = await planEmbed(repoRoot, config.embed);
    if (plan.pending.length > 0 && !quiet) {
      printEmbedPlan(plan);
    }
  }

  // ── 5. Report ─────────────────────────────────────────────────────────────
  if (!quiet) {
    // Count from NDJSON for the user-facing message — these should match the
    // kkernel report; if they diverge a sync round-trip is broken.
    const entityCount = await countLines(`${repoRoot}/${ENTITIES_FILE}`);
    const edgeCount = await countLines(`${repoRoot}/${EDGES_FILE}`);
    if (entityCount !== report.entities || edgeCount !== report.edges) {
      console.warn(
        `Warning: NDJSON count (${entityCount} entities, ${edgeCount} edges) does ` +
          `not match kernel report (${report.entities} entities, ${report.edges} edges).`,
      );
    }
    console.log(`Synced: ${report.entities} entities, ${report.edges} edges -> ${report.db_path}`);
  }
}
