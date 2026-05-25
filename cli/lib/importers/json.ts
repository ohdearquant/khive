/**
 * JSON array adapter (ADR-036 §2 P0 — "JSON" format).
 *
 * Reads a JSON file containing an array of objects. Each object is either an
 * entity or an edge depending on which fields are present:
 *   - has source + target  → edge
 *   - otherwise            → entity (name required)
 *
 * Entity fields recognized case-insensitively (ADR-036 §JSON-detection):
 *   id, name, kind, description, tags.
 * Everything else collects into `properties`. Edge fields recognized:
 *   edge_id, source, target, relation, weight; everything else → properties.
 *
 * Fatal errors (throw): JSON parse errors, non-array top level, missing
 * required fields (name, kind/defaultKind). These are never silently promoted
 * to empty results — the caller must handle them atomically.
 */

import type { EdgeRecord, EntityRecord } from "./types.ts";
import { randomUuid } from "./util.ts";

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
  "properties",
]);
const EDGE_RESERVED_LOWER = new Set([
  "edge_id",
  "source",
  "target",
  "relation",
  "weight",
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

    // Edge detection: has source + target (case-insensitive).
    const sourceVal = getField(obj, lm, "source");
    const targetVal = getField(obj, lm, "target");
    if (typeof sourceVal === "string" && typeof targetVal === "string") {
      // Missing relation on an otherwise-valid edge object is fatal.
      const relationVal = getField(obj, lm, "relation");
      if (!relationVal || typeof relationVal !== "string" || !relationVal.trim()) {
        throw new Error(`item ${i}: edge object is missing required "relation" field`);
      }
      const edge = extractEdge(obj, lm);
      if (edge) edges.push(edge);
      else {
        throw new Error(`item ${i}: edge has empty source/target/relation`);
      }
      continue;
    }

    // Entity: missing name or kind (without defaultKind) is fatal.
    const nameVal = getField(obj, lm, "name");
    if (!nameVal || typeof nameVal !== "string" || !nameVal.trim()) {
      throw new Error(`item ${i}: entity object is missing required "name" field`);
    }
    const kindVal = getField(obj, lm, "kind");
    const kindRaw = typeof kindVal === "string" ? kindVal.trim() : "";
    if (!kindRaw && !defaultKind) {
      throw new Error(
        `item ${i}: entity object is missing "kind" field and no --default-kind was specified`,
      );
    }

    const entity = extractEntity(obj, lm, defaultKind);
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
): EntityRecord | null {
  const idVal = getField(obj, lm, "id");
  const id = typeof idVal === "string" && idVal.length > 0 ? idVal : randomUuid();

  const nameVal = getField(obj, lm, "name");
  const name = typeof nameVal === "string" ? nameVal.trim() : "";
  if (!name) return null;

  const kindVal = getField(obj, lm, "kind");
  const kindRaw = typeof kindVal === "string" ? kindVal.trim() : "";
  const kind = kindRaw || defaultKind;
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

  const record: EntityRecord = { id, name, kind, properties };
  if (description !== undefined) record.description = description;
  if (tags !== undefined) record.tags = tags;
  return record;
}

function extractEdge(
  obj: Record<string, unknown>,
  lm: Map<string, string>,
): EdgeRecord | null {
  const sourceVal = getField(obj, lm, "source");
  const targetVal = getField(obj, lm, "target");
  const relationVal = getField(obj, lm, "relation");

  const source = String(sourceVal ?? "").trim();
  const target = String(targetVal ?? "").trim();
  const relation = String(relationVal ?? "").trim();
  if (!source || !target || !relation) return null;

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
  return { edge_id, source, target, relation, weight, properties };
}
