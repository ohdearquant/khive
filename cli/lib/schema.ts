/**
 * Schema loading and validation for .khive/kg/schema.yaml (ADR-048).
 *
 * schema.yaml is a simple, hand-maintained file.  We parse it with
 * @std/yaml and validate the result against the expected shape.
 *
 * Expected shape:
 *
 *   format_version: "1"
 *   entity_kinds:
 *     - concept
 *     - document
 *     ...
 *   edge_relations:
 *     - relation: contains
 *       description: "..."
 *     ...
 *   note_kinds:          # optional
 *     - observation
 *     ...
 *   remotes:             # optional
 *     upstream:
 *       url: "https://..."
 *       ref: "main"
 *   packs:               # optional
 *     - name: gtd
 *       version: "0.1"
 */

import { parse as parseYaml } from "@std/yaml";
import { SCHEMA_FILE } from "./paths.ts";

// ─── Default schema template (ADR-048 §3 + ADR-001 + ADR-002) ────────────────

export const DEFAULT_SCHEMA_YAML = `\
format_version: "1.0.0"
ontology_version: "1.0.0"
entity_kinds:
  - concept
  - document
  - dataset
  - project
  - person
  - org
edge_relations:
  - relation: contains
    category: structure
  - relation: part_of
    category: structure
  - relation: instance_of
    category: structure
  - relation: extends
    category: derivation
  - relation: variant_of
    category: derivation
  - relation: introduced_by
    category: derivation
  - relation: supersedes
    category: derivation
  - relation: depends_on
    category: dependency
  - relation: enables
    category: dependency
  - relation: implements
    category: implementation
  - relation: competes_with
    category: lateral
  - relation: composed_with
    category: lateral
  - relation: annotates
    category: annotation
note_kinds:
  - observation
  - insight
  - question
  - decision
  - reference
remotes: []
`;

// ─── Types ────────────────────────────────────────────────────────────────────

export interface EdgeRelationDef {
  relation: string;
  description?: string;
}

/**
 * A remote KG reference (ADR-037 §Reference syntax).
 *
 * Fields `url`, `ref`, and `namespace` are required (ADR-037 §schema.yaml remotes section).
 * `pin` is optional: a SHA-256 content hash (`sha256:<64hexchars>`); when present,
 * sync verifies the fetched archive against this hash before accepting it.
 *
 * Note: the legacy `repo`/`path`/`commit` field shape from ADR-020 v0 is superseded
 * by this `url`/`ref`/`namespace`/`pin` shape. Schema validation rejects the old shape.
 */
export interface RemoteDef {
  name: string;
  /** Git remote URL (required). */
  url: string;
  /** Branch or tag to resolve against (required). */
  ref: string;
  /** Namespace scoping entity resolution for this remote (required). */
  namespace: string;
  /**
   * Optional SHA-256 content hash pin (`sha256:<64hexchars>`).
   * When present, sync is mandatory-verify (ADR-037 §pin format).
   */
  pin?: string;
}

export interface PackRef {
  name: string;
  version?: string;
}

export interface Schema {
  format_version: string;
  entity_kinds: string[];
  edge_relations: EdgeRelationDef[];
  note_kinds?: string[];
  /** Remotes are a list of {name, url, ref, namespace, pin?} entries (ADR-037 §remotes). */
  remotes?: RemoteDef[];
  packs?: PackRef[];
}

export interface ValidationError {
  file: string;
  line: number;
  message: string;
}

// ─── YAML parser (delegates to @std/yaml) ────────────────────────────────────

function parseSchemaYaml(text: string): Schema {
  const parsed = parseYaml(text);
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new Error(`${SCHEMA_FILE} must be a YAML mapping`);
  }

  const obj = parsed as Record<string, unknown>;
  return {
    format_version: String(obj["format_version"] ?? ""),
    entity_kinds: Array.isArray(obj["entity_kinds"]) ? obj["entity_kinds"].map(String) : [],
    edge_relations: Array.isArray(obj["edge_relations"])
      ? obj["edge_relations"].map((rel) =>
        typeof rel === "string" ? { relation: rel } : rel as EdgeRelationDef
      )
      : [],
    note_kinds: Array.isArray(obj["note_kinds"]) ? obj["note_kinds"].map(String) : undefined,
    remotes: Array.isArray(obj["remotes"]) ? obj["remotes"] as RemoteDef[] : undefined,
    packs: Array.isArray(obj["packs"])
      ? obj["packs"].map((pack) => typeof pack === "string" ? { name: pack } : pack as PackRef)
      : undefined,
  };
}

// ─── Schema loading ───────────────────────────────────────────────────────────

/**
 * Load and parse schema.yaml from the repo root.
 * Throws if the file does not exist or cannot be parsed.
 */
export async function loadSchema(repoRoot: string): Promise<Schema> {
  const path = `${repoRoot}/${SCHEMA_FILE}`;
  const text = await Deno.readTextFile(path);
  return parseSchemaYaml(text);
}

// ─── Schema structural validation ────────────────────────────────────────────

/**
 * Validate a loaded Schema object for structural correctness.
 * Returns a list of ValidationErrors (empty = valid).
 */
export function validateSchema(schema: Schema): ValidationError[] {
  const errors: ValidationError[] = [];

  if (!schema.format_version) {
    errors.push({
      file: SCHEMA_FILE,
      line: 0,
      message: "Missing required field: format_version",
    });
  }

  if (!Array.isArray(schema.entity_kinds) || schema.entity_kinds.length === 0) {
    errors.push({
      file: SCHEMA_FILE,
      line: 0,
      message: "entity_kinds must be a non-empty list",
    });
  }

  if (!Array.isArray(schema.edge_relations) || schema.edge_relations.length === 0) {
    errors.push({
      file: SCHEMA_FILE,
      line: 0,
      message: "edge_relations must be a non-empty list",
    });
  }

  for (const rel of schema.edge_relations ?? []) {
    if (!rel.relation || typeof rel.relation !== "string") {
      errors.push({
        file: SCHEMA_FILE,
        line: 0,
        message: "Each edge_relations entry must have a non-empty 'relation' string",
      });
    }
  }

  return errors;
}
