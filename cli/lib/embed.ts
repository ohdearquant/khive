/**
 * Embedding pipeline subroutine (ADR-057 §5 + ADR-040).
 *
 * Phase C1 scope: the embed planner identifies entities that need embedding
 * based on the config and the on-disk NDJSON state. It does NOT call the
 * embedding runtime — that requires the Rust `lattice-embed` binary, which
 * is wired in Phase C2.
 *
 * Returned `EmbedPlan` is used by:
 *   - `khive kg embed --dry-run` (prints the plan)
 *   - `khive kg commit` / `khive kg sync` (when `embed.auto_embed = true`,
 *     prints the plan; the actual embedding is a no-op in Phase C1)
 *
 * Phase C2 will add `embedEntities(plan, embedConfig)` which calls
 * `lattice-embed` in batches of `embed.batch_size` and writes vectors to
 * `working.db#entities_vec`.
 */

import type { EmbedConfig } from "./config.ts";
export { ALLOWED_FIELDS, validateEmbedFields } from "./config.ts";
import { ENTITIES_FILE } from "./paths.ts";
import { parseEntityLine, readNdjson } from "./ndjson.ts";

// ─── Plan types ───────────────────────────────────────────────────────────────

export interface EmbedPlanEntry {
  /** Entity UUID. */
  id: string;
  /** Entity kind (concept, document, etc.). */
  kind: string;
  /** Text to embed, derived from `embed.fields.include`. */
  text: string;
}

export interface EmbedPlan {
  /** Total entities scanned. */
  total: number;
  /** Entities the planner would embed (Phase C1: all entities). */
  pending: EmbedPlanEntry[];
  /** Embedding model id from config. */
  model: string;
  /** Vector dimensions from config. */
  dimensions: number;
  /** Batch size (chunks per `lattice-embed` invocation). */
  batchSize: number;
  /** Field names concatenated into `text`. */
  fields: string[];
}

// ─── Plan builder ─────────────────────────────────────────────────────────────

/**
 * Walk `entities.ndjson` and build an embedding plan.
 *
 * The `text` for each entity is the concatenation of the configured fields
 * (defaults: `name`, `description`), joined by a single space (ADR-057 §5).
 * Empty fields are
 * skipped. Entities with no embeddable content are excluded from the plan.
 *
 * In Phase C1 every entity is considered "pending" — there is no working DB
 * to compare against. In Phase C2 the planner will query `working.db` for
 * the (entity_id, model_id) pair and skip already-embedded entities.
 */
export async function planEmbed(
  repoRoot: string,
  config: EmbedConfig,
): Promise<EmbedPlan> {
  const entitiesPath = `${repoRoot}/${ENTITIES_FILE}`;
  const pending: EmbedPlanEntry[] = [];
  let total = 0;

  try {
    for await (const entry of readNdjson(entitiesPath)) {
      if (entry.data === null) continue;
      total++;

      const e = parseEntityLine(entry.data);
      if (!e) continue;

      const parts: string[] = [];
      for (const field of config.fields.include) {
        const value = readField(entry.data, field);
        if (value && value.length > 0) parts.push(value);
      }

      const text = parts.join(" ").trim();
      if (text.length === 0) continue;

      pending.push({ id: e.id, kind: e.kind, text });
    }
  } catch (err) {
    if (!(err instanceof Deno.errors.NotFound)) {
      throw err;
    }
  }

  return {
    total,
    pending,
    model: config.model,
    dimensions: config.dimensions,
    batchSize: config.batch_size,
    fields: config.fields.include,
  };
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/**
 * Read a field from a parsed entity object. Looks in the top-level entity
 * first, then in `entity.properties` (where most descriptive fields live).
 * Returns the string form of the value, or an empty string if missing.
 */
function readField(data: Record<string, unknown>, field: string): string {
  if (field in data) {
    const v = data[field];
    if (typeof v === "string") return v;
    if (typeof v === "number" || typeof v === "boolean") return String(v);
  }
  const props = data["properties"];
  if (props && typeof props === "object" && field in (props as Record<string, unknown>)) {
    const v = (props as Record<string, unknown>)[field];
    if (typeof v === "string") return v;
    if (typeof v === "number" || typeof v === "boolean") return String(v);
  }
  return "";
}

// ─── Plan printer ─────────────────────────────────────────────────────────────

/**
 * Pretty-print an EmbedPlan to stdout. Used by `--dry-run` and the
 * commit/sync auto-embed banner.
 */
export function printEmbedPlan(
  plan: EmbedPlan,
  quiet = false,
  label = "Embed dry-run plan",
): void {
  if (quiet) return;
  if (plan.pending.length === 0) {
    console.log(
      `Embeddings are up-to-date, nothing to do. ` +
        `(${plan.total} entities scanned, model=${plan.model})`,
    );
    return;
  }
  console.log(
    `${label}: ${plan.pending.length}/${plan.total} entities pending ` +
      `(model=${plan.model}, dims=${plan.dimensions}, batch=${plan.batchSize})`,
  );
  const sample = plan.pending.slice(0, 3);
  for (const e of sample) {
    const preview = e.text.length > 60 ? `${e.text.slice(0, 60)}…` : e.text;
    console.log(`  ${e.id.slice(0, 8)}  ${e.kind.padEnd(10)} ${preview}`);
  }
  if (plan.pending.length > 3) {
    console.log(`  ... and ${plan.pending.length - 3} more`);
  }
}
