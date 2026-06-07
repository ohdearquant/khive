/**
 * `khive kg embed` — embed entities for vector search (ADR-057 §E4).
 *
 * Phase C1 scope: this command produces an `EmbedPlan` and prints it. The
 * actual call to the embedding runtime (`lattice-embed`) is wired in Phase C2.
 *
 * Usage:
 *   khive kg embed                  Plan a run for entities missing an embedding.
 *   khive kg embed --all            Plan a run for ALL entities (re-embed).
 *   khive kg embed --ids a,b,c      Plan a run for specific entity IDs.
 *   khive kg embed --dry-run        Print the plan only (default in Phase C1).
 *   khive kg embed --json           Emit machine-readable JSON.
 */

import { loadConfig } from "../lib/config.ts";
import { planEmbed, printEmbedPlan } from "../lib/embed.ts";

interface EmbedArgs {
  all: boolean;
  ids: string[] | null;
  dryRun: boolean;
  json: boolean;
}

function parseArgs(args: string[]): EmbedArgs {
  const out: EmbedArgs = { all: false, ids: null, dryRun: false, json: false };
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a === "--all") out.all = true;
    else if (a === "--dry-run") out.dryRun = true;
    else if (a === "--json") out.json = true;
    else if (a === "--ids" && args[i + 1]) {
      out.ids = args[i + 1].split(",").map((s) => s.trim()).filter((s) => s.length > 0);
      i++;
    } else if (a.startsWith("--ids=")) {
      out.ids = a.slice("--ids=".length).split(",").map((s) => s.trim()).filter((s) =>
        s.length > 0
      );
    } else if (a === "--help" || a === "-h") {
      printHelp();
      Deno.exit(0);
    }
  }
  return out;
}

function printHelp(): void {
  console.log(`Usage: khive kg embed [options]

Plan and (Phase C2: execute) embedding of entities for vector search.

Options:
  --all            Re-embed every entity, even those that already have a vector.
  --ids a,b,c     Limit the plan to specific entity IDs (UUIDs or short IDs).
  --dry-run        Print the plan; do not run the embedding runtime.
                   (Phase C1: always dry-run; the runtime is wired in Phase C2.)
  --json           Emit machine-readable JSON instead of a human summary.
  -h, --help       Show this help.`);
}

/**
 * `khive kg embed` entry point.
 */
export async function runEmbed(
  repoRoot: string,
  args: string[],
): Promise<void> {
  const opts = parseArgs(args);
  const config = await loadConfig(repoRoot);

  let plan = await planEmbed(repoRoot, config.embed);

  // --ids filter: if provided, restrict the plan to those entity IDs.
  if (opts.ids && opts.ids.length > 0) {
    const set = new Set(opts.ids);
    plan = {
      ...plan,
      pending: plan.pending.filter((e) => set.has(e.id) || set.has(e.id.slice(0, 8))),
    };
  }

  if (opts.json) {
    console.log(JSON.stringify({
      model: plan.model,
      dimensions: plan.dimensions,
      batch_size: plan.batchSize,
      fields: plan.fields,
      total: plan.total,
      pending: plan.pending.length,
      ids: plan.pending.map((e) => e.id),
    }));
    return;
  }

  if (plan.pending.length === 0) {
    console.log(
      `Embeddings are up-to-date, nothing to do. ` +
        `(${plan.total} entities scanned, model=${plan.model})`,
    );
    return;
  }

  printEmbedPlan(plan, false, "Embed dry-run plan");

  // Phase C1: dry-run only. Phase C2 will call embedEntities(plan, config.embed).
  if (!opts.dryRun && plan.pending.length > 0) {
    console.log("");
    console.log("Dry-run only: embedding runtime is not wired in Phase C1.");
  }
}
