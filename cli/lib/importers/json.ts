/**
 * JSON array adapter (ADR-036 §2 P0 — "JSON" format).
 *
 * Reads a JSON file containing an array of objects. Each object is either an
 * entity or an edge depending on which fields are present:
 *   - complete source + target signature  → edge
 *   - otherwise                           → entity (name required)
 *
 * Entity fields recognized case-insensitively (ADR-036 §JSON-detection):
 *   id, name, kind, description, tags, created_at, updated_at.
 * Everything else collects into `properties`. Edge fields recognized:
 *   edge_id, source, target, relation, weight, created_at, updated_at;
 * everything else → properties.
 *
 * Fatal errors (throw): JSON parse errors, non-array top level, missing
 * required fields (name, kind/defaultKind). These are never silently promoted
 * to empty results — the caller must handle them atomically.
 */

import type { EdgeRecord, EntityRecord } from "./types.ts";
import { randomUuid } from "./util.ts";
import { isRfc3339Timestamp } from "../rfc3339.ts";

export interface JsonImportResult {
  entities: EntityRecord[];
  edges: EdgeRecord[];
  warnings: string[];
}

const ENTITY_RESERVED_LOWER = new Set([
  "id",
  "name",
  "kind",
  "description",
  "tags",
  "created_at",
  "updated_at",
  "properties",
]);
const EDGE_RESERVED_LOWER = new Set([
  "edge_id",
  "source",
  "target",
  "relation",
  "weight",
  "created_at",
  "updated_at",
  "properties",
]);

/**
 * Build a case-insensitive lookup map from a raw object's keys.
 * The map value is the raw key (preserving original casing), keyed by lowercase.
 */
function buildLowerMap(obj: Record<string, unknown>): Map<string, string> {
  const m = new Map<string, string>();
  for (const k of Object.keys(obj)) {
    m.set(k.toLowerCase(), k);
  }
  return m;
}

/** Get a field from obj by lowercase key name (case-insensitive). */
function getField(
  obj: Record<string, unknown>,
  lowerMap: Map<string, string>,
  lowerKey: string,
): unknown {
  const rawKey = lowerMap.get(lowerKey);
  if (rawKey === undefined) return undefined;
  return obj[rawKey];
}

function extractOptionalTimestamp(
  obj: Record<string, unknown>,
  lowerMap: Map<string, string>,
  index: number,
  field: string,
): string | undefined {
  const rawKey = lowerMap.get(field);
  if (rawKey === undefined) return undefined;
  const value = obj[rawKey];
  if (!isRfc3339Timestamp(value)) {
    throw new Error(`item ${index}: "${field}" must be an RFC3339 string`);
  }
  return value;
}

export function adaptJson(
  text: string,
  defaultKind?: string,
): JsonImportResult {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch (err) {
    throw new Error(`JSON parse error: ${(err as Error).message}`);
  }
  if (!Array.isArray(parsed)) {
    throw new Error("JSON adapter expects a top-level array of objects");
  }

  const entities: EntityRecord[] = [];
  const edges: EdgeRecord[] = [];
  const warnings: string[] = [];

  for (let i = 0; i < parsed.length; i++) {
    const item = parsed[i];
    // Non-object items are a fatal structural error (ADR-036 §5: all-or-nothing).
    if (!item || typeof item !== "object" || Array.isArray(item)) {
      throw new Error(
        `item ${i}: expected a JSON object, got ${Array.isArray(item) ? "array" : typeof item}`,
      );
    }
    const obj = item as Record<string, unknown>;
    const lm = buildLowerMap(obj);

    // Dispatch only on complete canonical signatures. `from`/`to` remain
    // ordinary entity properties. Supplying both signatures is ambiguous.
    const entitySignature = lm.has("kind") && lm.has("name");
    const edgeSignature = lm.has("source") && lm.has("target");
    if (entitySignature && edgeSignature) {
      throw new Error(
        `item ${i}: ambiguous record has complete entity (kind + name) and edge (source + target) signatures`,
      );
    }

    if (edgeSignature) {
      for (const field of ["source", "target", "relation"]) {
        const value = getField(obj, lm, field);
        if (typeof value !== "string" || !value.trim()) {
          throw new Error(`item ${i}: edge "${field}" must be a non-blank string`);
        }
      }
      edges.push(extractEdge(obj, lm, i));
      continue;
    }

    // Entity: missing name or kind (without defaultKind) is fatal.
    const nameVal = getField(obj, lm, "name");
    if (!nameVal || typeof nameVal !== "string" || !nameVal.trim()) {
      throw new Error(`item ${i}: entity object is missing required "name" field`);
    }
    const kindVal = getField(obj, lm, "kind");
    if (lm.has("kind") && (typeof kindVal !== "string" || !kindVal.trim())) {
      throw new Error(`item ${i}: entity "kind" must be a non-blank string`);
    }
    if (!lm.has("kind") && (typeof defaultKind !== "string" || !defaultKind.trim())) {
      throw new Error(
        `item ${i}: entity object is missing "kind" field and no --default-kind was specified`,
      );
    }

    const entity = extractEntity(obj, lm, defaultKind, i);
    if (entity) {
      entities.push(entity);
    } else {
      // extractEntity returning null is an internal inconsistency after the above checks.
      throw new Error(`item ${i}: failed to extract entity (internal error)`);
    }
  }

  return { entities, edges, warnings };
}

function extractEntity(
  obj: Record<string, unknown>,
  lm: Map<string, string>,
  defaultKind: string | undefined,
  index: number,
): EntityRecord | null {
  const idVal = getField(obj, lm, "id");
  const id = typeof idVal === "string" && idVal.length > 0 ? idVal : randomUuid();

  const nameVal = getField(obj, lm, "name");
  const name = typeof nameVal === "string" ? nameVal : "";
  if (!name.trim()) return null;

  const kindVal = getField(obj, lm, "kind");
  const kindRaw = typeof kindVal === "string" ? kindVal.trim() : "";
  const kind = kindRaw || defaultKind?.trim();
  if (!kind) return null;

  // description is a top-level field (ADR-048), not a property.
  const descVal = getField(obj, lm, "description");
  const description = typeof descVal === "string" && descVal.length > 0 ? descVal : undefined;

  const properties: Record<string, unknown> = {};
  // Existing properties object — merge first.
  const propsVal = getField(obj, lm, "properties");
  if (propsVal && typeof propsVal === "object" && !Array.isArray(propsVal)) {
    for (const [k, v] of Object.entries(propsVal as Record<string, unknown>)) {
      properties[k] = v;
    }
  }
  // All non-reserved fields go into properties.
  for (const [k, v] of Object.entries(obj)) {
    if (ENTITY_RESERVED_LOWER.has(k.toLowerCase())) continue;
    if (v === undefined || v === null) continue;
    properties[k] = v;
  }

  const tagsVal = getField(obj, lm, "tags");
  const tags = Array.isArray(tagsVal)
    ? tagsVal.filter((t): t is string => typeof t === "string")
    : undefined;

  const created_at = extractOptionalTimestamp(obj, lm, index, "created_at");
  const updated_at = extractOptionalTimestamp(obj, lm, index, "updated_at");

  const record: EntityRecord = { id, name, kind, properties };
  if (description !== undefined) record.description = description;
  if (tags !== undefined) record.tags = tags;
  if (created_at !== undefined) record.created_at = created_at;
  if (updated_at !== undefined) record.updated_at = updated_at;
  return record;
}

function extractEdge(
  obj: Record<string, unknown>,
  lm: Map<string, string>,
  index: number,
): EdgeRecord {
  const sourceVal = getField(obj, lm, "source");
  const targetVal = getField(obj, lm, "target");
  const relationVal = getField(obj, lm, "relation");

  // The dispatch loop already validates a trimmed view; retain the caller's
  // exact accepted identity bytes so canonical validation can reject rather
  // than silently coerce a whitespace-wrapped UUID or remote reference.
  const source = typeof sourceVal === "string" ? sourceVal : "";
  const target = typeof targetVal === "string" ? targetVal : "";
  const relation = String(relationVal ?? "").trim();

  const edgeIdVal = getField(obj, lm, "edge_id");
  const edge_id = typeof edgeIdVal === "string" && edgeIdVal.length > 0 ? edgeIdVal : randomUuid();

  const weightVal = getField(obj, lm, "weight");
  const weight = typeof weightVal === "number" ? weightVal : 0.7;

  const properties: Record<string, unknown> = {};
  const propsVal = getField(obj, lm, "properties");
  if (propsVal && typeof propsVal === "object" && !Array.isArray(propsVal)) {
    for (const [k, v] of Object.entries(propsVal as Record<string, unknown>)) {
      properties[k] = v;
    }
  }
  for (const [k, v] of Object.entries(obj)) {
    if (EDGE_RESERVED_LOWER.has(k.toLowerCase())) continue;
    if (v === undefined || v === null) continue;
    properties[k] = v;
  }
  const created_at = extractOptionalTimestamp(obj, lm, index, "created_at");
  const updated_at = extractOptionalTimestamp(obj, lm, index, "updated_at");
  return { edge_id, source, target, relation, weight, properties, created_at, updated_at };
}
