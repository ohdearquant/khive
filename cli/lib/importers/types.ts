/**
 * Shared types for the format adapters under cli/lib/importers/ (ADR-036).
 *
 * Each adapter returns these records; the dispatcher merges them and writes
 * them as sorted NDJSON via the standard `khive kg import` pipeline.
 */

export interface EntityRecord {
  id: string;
  name: string;
  kind: string;
  /** Top-level field per ADR-048. undefined means absent from source; canonicalEntityJson emits null. */
  description?: string;
  properties: Record<string, unknown>;
  /** Top-level field per ADR-048. undefined means absent from source; canonicalEntityJson emits []. */
  tags?: string[];
}

export interface EdgeRecord {
  edge_id: string;
  source: string;
  target: string;
  relation: string;
  weight?: number;
  properties: Record<string, unknown>;
}
