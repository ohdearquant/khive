/**
 * NDJSON (newline-delimited JSON) utilities for the KG file layer (ADR-048).
 *
 * Each line in entities.ndjson / edges.ndjson is one JSON object.
 * Blank lines and lines beginning with '#' are skipped (comment/padding convention).
 */

import { EDGES_FILE, ENTITIES_FILE } from "./paths.ts";

// ─── Domain types ────────────────────────────────────────────────────────────

/** The 6 entity kinds (ADR-001). Closed set — do not extend without an ADR. */
export const ENTITY_KINDS = [
  "concept",
  "document",
  "dataset",
  "project",
  "person",
  "org",
] as const;

export type EntityKind = (typeof ENTITY_KINDS)[number];

/** The 13 edge relations (ADR-002). Closed set — do not extend without an ADR. */
export const EDGE_RELATIONS = [
  "contains",
  "part_of",
  "instance_of",
  "extends",
  "variant_of",
  "introduced_by",
  "supersedes",
  "depends_on",
  "enables",
  "implements",
  "competes_with",
  "composed_with",
  "annotates",
] as const;

export type EdgeRelation = (typeof EDGE_RELATIONS)[number];

/** The 5 note kinds (ADR-019). Closed set. */
export const NOTE_KINDS = [
  "observation",
  "insight",
  "question",
  "decision",
  "reference",
] as const;

export type NoteKind = (typeof NOTE_KINDS)[number];

/** A validated entity record. */
export interface Entity {
  id: string;
  name: string;
  kind: EntityKind;
  [key: string]: unknown;
}

/** A validated edge record (ADR-048 §2 field names). */
export interface Edge {
  edge_id: string;
  source: string;
  target: string;
  relation: EdgeRelation;
  [key: string]: unknown;
}

// ─── UUID validation ──────────────────────────────────────────────────────────

/** UUID check: full 8-4-4-4-12 hex format required for NDJSON records. */
function isUuid(value: string): boolean {
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i
    .test(value);
}

// ─── Parsing helpers ──────────────────────────────────────────────────────────

/**
 * Attempt to parse a raw JSON value as an Entity.
 * Returns null if required fields are missing or invalid.
 */
export function parseEntityLine(json: unknown): Entity | null {
  if (typeof json !== "object" || json === null || Array.isArray(json)) {
    return null;
  }
  const obj = json as Record<string, unknown>;
  if (typeof obj["id"] !== "string" || !isUuid(obj["id"])) return null;
  if (typeof obj["name"] !== "string" || obj["name"].length === 0) return null;
  if (!ENTITY_KINDS.includes(obj["kind"] as EntityKind)) return null;
  return obj as Entity;
}

/**
 * Attempt to parse a raw JSON value as an Edge (ADR-048 §2 field names).
 *
 * Required fields: edge_id (UUID), source (UUID or remote ref), target (UUID or remote ref),
 * relation (closed set from EDGE_RELATIONS).
 */
export function parseEdgeLine(json: unknown): Edge | null {
  if (typeof json !== "object" || json === null || Array.isArray(json)) {
    return null;
  }
  const obj = json as Record<string, unknown>;
  if (typeof obj["edge_id"] !== "string" || !isUuid(obj["edge_id"])) return null;
  if (typeof obj["source"] !== "string" || obj["source"].length === 0) {
    return null;
  }
  if (typeof obj["target"] !== "string" || obj["target"].length === 0) {
    return null;
  }
  if (!EDGE_RELATIONS.includes(obj["relation"] as EdgeRelation)) return null;
  return obj as Edge;
}

// ─── File I/O ─────────────────────────────────────────────────────────────────

/**
 * Iterate over all non-blank, non-comment lines in an NDJSON file.
 * Yields `{ line, data }` for each successfully parsed JSON line.
 * Lines that fail JSON.parse yield `{ line, data: null, error }` instead of throwing.
 */
export async function* readNdjson(
  path: string,
): AsyncIterable<
  { line: number; data: Record<string, unknown>; error?: undefined } | {
    line: number;
    data: null;
    error: string;
  }
> {
  let text: string;
  try {
    text = await Deno.readTextFile(path);
  } catch (err) {
    if (err instanceof Deno.errors.NotFound) return;
    throw err;
  }

  const lines = text.split("\n");
  for (let i = 0; i < lines.length; i++) {
    const raw = lines[i].trim();
    if (raw === "" || raw.startsWith("#")) continue;
    try {
      const data = JSON.parse(raw) as Record<string, unknown>;
      yield { line: i + 1, data };
    } catch {
      yield { line: i + 1, data: null, error: `Invalid JSON on line ${i + 1}` };
    }
  }
}

/**
 * Count the number of data lines (non-blank, non-comment) in an NDJSON file.
 * Returns 0 if the file does not exist.
 */
export async function countLines(path: string): Promise<number> {
  let text: string;
  try {
    text = await Deno.readTextFile(path);
  } catch (err) {
    if (err instanceof Deno.errors.NotFound) return 0;
    throw err;
  }
  let count = 0;
  for (const line of text.split("\n")) {
    const raw = line.trim();
    if (raw !== "" && !raw.startsWith("#")) count++;
  }
  return count;
}

/**
 * Read all entity lines from entities.ndjson, returning the Set of entity IDs
 * and total count. Used for cross-checks in validation.
 */
export async function readAllEntities(
  repoRoot: string,
): Promise<{ ids: Set<string>; count: number }> {
  const path = `${repoRoot}/${ENTITIES_FILE}`;
  const ids = new Set<string>();
  let count = 0;
  for await (const { data } of readNdjson(path)) {
    const e = parseEntityLine(data);
    if (e) {
      ids.add(e.id);
      count++;
    }
  }
  return { ids, count };
}

/**
 * Read all edge lines from edges.ndjson, returning the count.
 */
export function readEdgeCount(repoRoot: string): Promise<number> {
  const path = `${repoRoot}/${EDGES_FILE}`;
  return countLines(path);
}
