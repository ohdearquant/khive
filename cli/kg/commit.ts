/**
 * `khive kg commit` — validate + git commit (Phase C1).
 *
 * Phase C1 scope: validate the existing NDJSON files, stage them, and create a git commit.
 * Export from a live DB (the `khive kg export` step) is Phase C2 and is not yet integrated.
 * Until Phase C2, this command operates on NDJSON files that are managed directly by the
 * author (not generated from a Rust DB).
 */

import { exec, getCurrentBranch, gitAdd, gitCommit } from "../lib/git.ts";
import { loadConfig } from "../lib/config.ts";
import { planEmbed, printEmbedPlan } from "../lib/embed.ts";
import { EDGES_FILE, ensureStateDir, ENTITIES_FILE, SCHEMA_FILE } from "../lib/paths.ts";
import { countLines } from "../lib/ndjson.ts";
import { printRuleViolations, printValidationResult, validateWithRules } from "./validate.ts";
import { RulesFileErrors } from "../lib/rules.ts";

// ─── Prompt helper ────────────────────────────────────────────────────────────

/** Read a single line from stdin. Returns empty string on EOF. */
async function readLine(): Promise<string> {
  const chunks: Uint8Array[] = [];
  while (true) {
    const buf = new Uint8Array(4096);
    const n = await Deno.stdin.read(buf);
    if (n === null) break;

    const chunk = buf.subarray(0, n);
    const newline = chunk.indexOf(10);
    if (newline !== -1) {
      chunks.push(chunk.subarray(0, newline));
      break;
    }
    chunks.push(chunk);
  }

  const total = chunks.reduce((sum, chunk) => sum + chunk.length, 0);
  const merged = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    merged.set(chunk, offset);
    offset += chunk.length;
  }
  return new TextDecoder().decode(merged).trim();
}

// ─── Staged-change check ──────────────────────────────────────────────────────

/**
 * Returns true if there are staged changes to .khive/kg/ files.
 * Uses `git diff --cached --quiet` — exit code 1 means changes are staged.
 */
async function hasStagedKgChanges(repoRoot: string): Promise<boolean> {
  const result = await exec([
    "git",
    "diff",
    "--cached",
    "--quiet",
    "--",
    `${repoRoot}/${ENTITIES_FILE}`,
    `${repoRoot}/${EDGES_FILE}`,
    `${repoRoot}/${SCHEMA_FILE}`,
  ]);
  // exit 0 = no staged changes, exit 1 = staged changes present
  return result.code === 1;
}

// ─── CLI entry point ──────────────────────────────────────────────────────────

/**
 * `khive kg commit` command.
 *
 * Args:
 *   -m <message>   Commit message (prompts if omitted).
 *
 * Exits 0 on success or when there is nothing to commit.
 * Exits 1 on validation failure or git error.
 */
export async function runCommit(repoRoot: string, args: string[]): Promise<void> {
  // Ensure .khive/state/ exists — works even after git clone without init
  await ensureStateDir(repoRoot);

  // ── 1. Parse -m flag ──────────────────────────────────────────────────────
  let message: string | undefined;
  const mIdx = args.indexOf("-m");
  if (mIdx !== -1 && args[mIdx + 1]) {
    message = args[mIdx + 1];
  }

  if (!message) {
    // Prompt via stdin
    console.log("Commit message: ");
    message = await readLine();
    if (!message) {
      console.error("Commit message is required.");
      Deno.exit(1);
    }
  }

  // ── 2. Snapshot current counts before any export/validation operation ─────
  const beforeEntityCount = await countLines(`${repoRoot}/${ENTITIES_FILE}`);
  const beforeEdgeCount = await countLines(`${repoRoot}/${EDGES_FILE}`);

  // ── 3. Export step (Phase C1: validation pass on existing NDJSON) ─────────
  // When `khive kg export` is available, this step will run:
  //   await runExport(repoRoot);
  // For now we validate the existing NDJSON files, which is the same
  // correctness guarantee for KG repos that manage NDJSON manually.
  console.log("Validating KG files...");
  let validationResult;
  try {
    validationResult = await validateWithRules(repoRoot);
  } catch (err) {
    if (err instanceof RulesFileErrors) {
      console.error("Commit aborted: rules.yaml is malformed:");
      for (const e of err.errors) {
        console.error(`  ${e.message}`);
      }
      Deno.exit(2);
    }
    throw err;
  }

  const ruleErrors = validationResult.ruleViolations.filter((v) => v.severity === "error");

  if (!validationResult.valid || ruleErrors.length > 0) {
    printValidationResult(validationResult);
    if (validationResult.ruleViolations.length > 0) {
      printRuleViolations(validationResult.ruleViolations);
    }
    console.error("\nCommit aborted: fix validation errors first.");
    Deno.exit(1);
  }

  // ── 3b. Embed step (ADR-057 §E3, Phase C1: plan only) ─────────────────────
  // When `embed.auto_embed = true` (the default), print an embed plan so
  // commits surface entities awaiting vectorization. Embedding execution is
  // Phase C2 — wired when `lattice-embed` is available.
  const config = await loadConfig(repoRoot);
  if (config.embed.auto_embed) {
    const plan = await planEmbed(repoRoot, config.embed);
    if (plan.pending.length > 0) {
      printEmbedPlan(plan);
    }
  }

  // ── 4. Stage KG files ─────────────────────────────────────────────────────
  await gitAdd([
    `${repoRoot}/${ENTITIES_FILE}`,
    `${repoRoot}/${EDGES_FILE}`,
    `${repoRoot}/${SCHEMA_FILE}`,
  ]);

  // ── 5. Check for staged changes ───────────────────────────────────────────
  const hasChanges = await hasStagedKgChanges(repoRoot);
  if (!hasChanges) {
    console.log("Nothing to commit (KG is clean)");
    return;
  }

  // ── 6. Git commit ─────────────────────────────────────────────────────────
  let shortSha: string;
  try {
    shortSha = await gitCommit(message);
  } catch (err) {
    console.error(`git commit failed: ${(err as Error).message}`);
    Deno.exit(1);
  }

  // ── 7. Print summary ──────────────────────────────────────────────────────
  const branch = await getCurrentBranch();
  const entityCount = await countLines(`${repoRoot}/${ENTITIES_FILE}`);
  const edgeCount = await countLines(`${repoRoot}/${EDGES_FILE}`);
  const entityChanged = entityCount - beforeEntityCount;
  const edgeChanged = edgeCount - beforeEdgeCount;
  const formatDelta = (delta: number): string => delta >= 0 ? `+${delta}` : `${delta}`;

  console.log(`[${branch} ${shortSha}] ${message}`);
  console.log(
    `  ${entityCount} entities, ${edgeCount} edges (${formatDelta(entityChanged)} entities, ${
      formatDelta(edgeChanged)
    } edges)`,
  );
}
