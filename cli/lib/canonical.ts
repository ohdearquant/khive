/**
 * Canonical JSON serializers for NDJSON entity and edge records (ADR-048 §2).
 *
 * ADR-048 requires fixed field ordering so that re-exporting the same logical
 * graph state always produces bit-identical bytes.  These helpers emit fields
 * in the ADR-specified order, sort property keys alphabetically, and sort tags
 * lexicographically.  Both serializers return a single-line JSON string ready
 * for NDJSON output (no trailing newline).
 */

// ─── Entity canonical serializer ─────────────────────────────────────────────

/**
 * Return a canonical single-line JSON string for an entity record.
 *
 * Field order (ADR-048 §2, entity shape):
 *   id, kind, name, description, properties, tags, created_at, updated_at
 *
 * Optional fields (description, created_at, updated_at) are omitted when
 * absent or null to match the ADR-048 compatibility note.
 */
export function canonicalEntityJson(e: Record<string, unknown>): string {
  const out: Record<string, unknown> = {};

  out["id"] = e["id"];
  out["kind"] = e["kind"];
  out["name"] = e["name"];

  if (e["description"] !== undefined && e["description"] !== null) {
    out["description"] = e["description"];
  }

  // Sort property keys alphabetically
  if (e["properties"] !== undefined && e["properties"] !== null) {
    const props = e["properties"] as Record<string, unknown>;
    const sortedProps: Record<string, unknown> = {};
    for (const k of Object.keys(props).sort()) {
      sortedProps[k] = props[k];
    }
    out["properties"] = sortedProps;
  } else {
    out["properties"] = {};
  }

  // Sort tags lexicographically
  if (Array.isArray(e["tags"])) {
    out["tags"] = [...(e["tags"] as string[])].sort();
  } else {
    out["tags"] = [];
  }

  if (e["created_at"] !== undefined && e["created_at"] !== null) {
    out["created_at"] = e["created_at"];
  }

  if (e["updated_at"] !== undefined && e["updated_at"] !== null) {
    out["updated_at"] = e["updated_at"];
  }

  return JSON.stringify(out);
}

// ─── Edge canonical serializer ────────────────────────────────────────────────

/**
 * Return a canonical single-line JSON string for an edge record.
 *
 * Field order (ADR-048 §2, edge shape):
 *   edge_id, source, target, relation, weight, properties, created_at, updated_at
 *
 * The `properties` field is always present (empty object when absent).
 * Optional fields (created_at, updated_at) are omitted when absent or null.
 */
export function canonicalEdgeJson(e: Record<string, unknown>): string {
  const out: Record<string, unknown> = {};

  out["edge_id"] = e["edge_id"];
  out["source"] = e["source"];
  out["target"] = e["target"];
  out["relation"] = e["relation"];

  if (e["weight"] !== undefined) {
    out["weight"] = e["weight"];
  }

  // Sort property keys alphabetically; always include the field
  if (e["properties"] !== undefined && e["properties"] !== null) {
    const props = e["properties"] as Record<string, unknown>;
    const sortedProps: Record<string, unknown> = {};
    for (const k of Object.keys(props).sort()) {
      sortedProps[k] = props[k];
    }
    out["properties"] = sortedProps;
  } else {
    out["properties"] = {};
  }

  if (e["created_at"] !== undefined && e["created_at"] !== null) {
    out["created_at"] = e["created_at"];
  }

  if (e["updated_at"] !== undefined && e["updated_at"] !== null) {
    out["updated_at"] = e["updated_at"];
  }

  return JSON.stringify(out);
}
