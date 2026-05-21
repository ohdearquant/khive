/**
 * Declarative pack manifest loader + validator (ADR-050 §1).
 *
 * A declarative pack is a directory containing a `pack.yaml` file. This module
 * parses and validates the manifest, returning a structured error report
 * suitable for both the `khive pack check` CLI command and the future Rust
 * runtime integration (ADR-050 §Implementation).
 */

import { parse as parseYaml } from "@std/yaml";

// ─── Manifest types ──────────────────────────────────────────────────────────

export interface PackEndpointEntry {
  relation: string;
  endpoints: Array<[string, string]>;
}

export interface PackPropertyEntry {
  key: string;
  values?: string[];
}

export interface PackManifest {
  name: string;
  version: string;
  description?: string;
  author?: string;
  license?: string;
  homepage?: string;
  entity_kinds?: string[];
  note_kinds?: string[];
  edge_endpoints?: PackEndpointEntry[];
  properties?: Record<string, PackPropertyEntry[]>;
}

export interface PackValidationError {
  path: string; // dot path into the manifest (e.g. "edge_endpoints[1].relation")
  message: string;
}

export interface PackValidationResult {
  valid: boolean;
  manifest: PackManifest | null;
  errors: PackValidationError[];
  warnings: PackValidationError[];
}

// ─── Closed sets (ADR-001, ADR-002) ──────────────────────────────────────────

const BASE_ENTITY_KINDS = new Set([
  "concept",
  "document",
  "dataset",
  "project",
  "person",
  "org",
]);

/**
 * Reserved substrate-level names that cannot be used as pack entity kinds.
 *
 * These are the substrate primitives (ADR-004: note, entity, event), the
 * substrate dispatch token used by the KG pack verb surface (edge), and the
 * base note kinds registered by built-in packs (ADR-019: observation, insight,
 * question, decision, reference; ADR-036: memory; ADR-026: task). Declarative
 * packs may only contribute new entity kinds — they cannot claim identifiers
 * that belong to the runtime's core taxonomy.
 */
const RESERVED_SUBSTRATE_NAMES = new Set([
  // Substrate primitives (ADR-004)
  "note",
  "entity",
  "event",
  // Substrate dispatch token used by KG verb surface
  "edge",
  // Base note kinds (ADR-019)
  "observation",
  "insight",
  "question",
  "decision",
  "reference",
  // Pack-registered note kinds (ADR-026 GTD, ADR-036 memory)
  "task",
  "memory",
]);

const BASE_EDGE_RELATIONS = new Set([
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
]);

const NAME_RE = /^[a-z][a-z0-9-]{0,62}$/;
const KIND_RE = /^[a-z][a-z0-9_]{0,62}$/;
const PROP_KEY_RE = /^[a-z][a-z0-9_]{0,62}$/;
const SEMVER_RE = /^\d+\.\d+\.\d+$/;

/**
 * Complete set of top-level keys permitted in a pack.yaml manifest (ADR-050 §1).
 * Unknown keys are rejected: declarative packs are vocabulary-only and cannot
 * claim unsupported fields such as `verbs` (which would require a Rust pack).
 */
const ALLOWED_TOP_LEVEL_KEYS = new Set([
  "name",
  "version",
  "description",
  "author",
  "license",
  "homepage",
  "entity_kinds",
  "note_kinds",
  "edge_endpoints",
  "properties",
]);

// ─── Validator ───────────────────────────────────────────────────────────────

export function validatePackManifest(raw: unknown): PackValidationResult {
  const errors: PackValidationError[] = [];
  const warnings: PackValidationError[] = [];

  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
    return {
      valid: false,
      manifest: null,
      errors: [{ path: "", message: "pack.yaml must be a YAML mapping at the top level" }],
      warnings: [],
    };
  }
  const obj = raw as Record<string, unknown>;

  // Reject unknown top-level keys (ADR-050 §1 — manifest is a closed schema).
  // `verbs` gets a specific message because it is the most likely mistake.
  for (const key of Object.keys(obj)) {
    if (!ALLOWED_TOP_LEVEL_KEYS.has(key)) {
      if (key === "verbs") {
        errors.push({
          path: key,
          message:
            "Declarative packs are vocabulary-only (ADR-050). Verb handlers require a Rust pack. Remove the `verbs` field from pack.yaml.",
        });
      } else {
        errors.push({
          path: key,
          message: `Unknown top-level key '${key}'. Allowed keys: ${
            [...ALLOWED_TOP_LEVEL_KEYS].join(", ")
          }`,
        });
      }
    }
  }

  // name
  if (typeof obj.name !== "string") {
    errors.push({ path: "name", message: "name is required and must be a string" });
  } else if (!NAME_RE.test(obj.name)) {
    errors.push({
      path: "name",
      message: `name '${obj.name}' must match ${NAME_RE}`,
    });
  }

  // version
  if (typeof obj.version !== "string") {
    errors.push({ path: "version", message: "version is required and must be a string" });
  } else if (!SEMVER_RE.test(obj.version)) {
    errors.push({
      path: "version",
      message: `version '${obj.version}' must be semver MAJOR.MINOR.PATCH`,
    });
  }

  // description, author, license, homepage are optional strings
  for (const k of ["description", "author", "license", "homepage"]) {
    if (obj[k] !== undefined && typeof obj[k] !== "string") {
      errors.push({ path: k, message: `${k} must be a string when present` });
    }
  }

  // entity_kinds
  const declaredKinds = new Set<string>();
  if (obj.entity_kinds !== undefined) {
    if (!Array.isArray(obj.entity_kinds)) {
      errors.push({ path: "entity_kinds", message: "entity_kinds must be an array" });
    } else {
      for (let i = 0; i < obj.entity_kinds.length; i++) {
        const k = obj.entity_kinds[i];
        if (typeof k !== "string") {
          errors.push({
            path: `entity_kinds[${i}]`,
            message: "entity kind must be a string",
          });
          continue;
        }
        if (!KIND_RE.test(k)) {
          errors.push({
            path: `entity_kinds[${i}]`,
            message: `kind '${k}' must match ${KIND_RE}`,
          });
        } else if (RESERVED_SUBSTRATE_NAMES.has(k)) {
          errors.push({
            path: `entity_kinds[${i}]`,
            message:
              `kind '${k}' is a reserved substrate name and cannot be declared as a pack entity kind (closed taxonomy — ADR-001, ADR-004, ADR-019)`,
          });
        } else if (BASE_ENTITY_KINDS.has(k)) {
          warnings.push({
            path: `entity_kinds[${i}]`,
            message:
              `kind '${k}' already in ADR-001 base set — declaration is redundant (idempotent)`,
          });
        }
        declaredKinds.add(k);
      }
    }
  }
  // Union with base kinds for endpoint validation.
  const allKnownKinds = new Set([...BASE_ENTITY_KINDS, ...declaredKinds]);

  // note_kinds
  if (obj.note_kinds !== undefined) {
    if (!Array.isArray(obj.note_kinds)) {
      errors.push({ path: "note_kinds", message: "note_kinds must be an array" });
    } else {
      for (let i = 0; i < obj.note_kinds.length; i++) {
        const k = obj.note_kinds[i];
        if (typeof k !== "string" || !KIND_RE.test(k)) {
          errors.push({
            path: `note_kinds[${i}]`,
            message: `note kind '${k}' must match ${KIND_RE}`,
          });
        }
      }
    }
  }

  // edge_endpoints
  if (obj.edge_endpoints !== undefined) {
    if (!Array.isArray(obj.edge_endpoints)) {
      errors.push({ path: "edge_endpoints", message: "edge_endpoints must be an array" });
    } else {
      for (let i = 0; i < obj.edge_endpoints.length; i++) {
        const entry = obj.edge_endpoints[i] as Record<string, unknown> | null;
        if (!entry || typeof entry !== "object") {
          errors.push({
            path: `edge_endpoints[${i}]`,
            message: "each entry must be a mapping",
          });
          continue;
        }
        const relation = entry.relation;
        const endpoints = entry.endpoints;
        if (typeof relation !== "string") {
          errors.push({
            path: `edge_endpoints[${i}].relation`,
            message: "relation must be a string",
          });
        } else if (!BASE_EDGE_RELATIONS.has(relation)) {
          errors.push({
            path: `edge_endpoints[${i}].relation`,
            message:
              `relation '${relation}' not in ADR-002 closed set (declarative packs cannot introduce new relation names)`,
          });
        }
        if (!Array.isArray(endpoints)) {
          errors.push({
            path: `edge_endpoints[${i}].endpoints`,
            message: "endpoints must be an array of [source_kind, target_kind] pairs",
          });
          continue;
        }
        for (let j = 0; j < endpoints.length; j++) {
          const pair = endpoints[j];
          if (
            !Array.isArray(pair) || pair.length !== 2 ||
            typeof pair[0] !== "string" || typeof pair[1] !== "string"
          ) {
            errors.push({
              path: `edge_endpoints[${i}].endpoints[${j}]`,
              message: "each endpoint must be [source_kind, target_kind]",
            });
            continue;
          }
          const [src, dst] = pair;
          if (!allKnownKinds.has(src)) {
            warnings.push({
              path: `edge_endpoints[${i}].endpoints[${j}][0]`,
              message:
                `source kind '${src}' not declared in this pack or ADR-001 (may resolve at runtime from another pack)`,
            });
          }
          if (!allKnownKinds.has(dst)) {
            warnings.push({
              path: `edge_endpoints[${i}].endpoints[${j}][1]`,
              message:
                `target kind '${dst}' not declared in this pack or ADR-001 (may resolve at runtime from another pack)`,
            });
          }
        }
      }
    }
  }

  // properties
  if (obj.properties !== undefined) {
    if (
      typeof obj.properties !== "object" || obj.properties === null ||
      Array.isArray(obj.properties)
    ) {
      errors.push({ path: "properties", message: "properties must be a mapping" });
    } else {
      const props = obj.properties as Record<string, unknown>;
      for (const [kind, entries] of Object.entries(props)) {
        if (!allKnownKinds.has(kind)) {
          warnings.push({
            path: `properties.${kind}`,
            message:
              `properties declared on kind '${kind}' which is not in this pack or ADR-001 base set`,
          });
        }
        if (!Array.isArray(entries)) {
          errors.push({
            path: `properties.${kind}`,
            message: "must be an array of {key, values?} entries",
          });
          continue;
        }
        for (let i = 0; i < entries.length; i++) {
          const e = entries[i] as Record<string, unknown> | null;
          if (!e || typeof e !== "object") {
            errors.push({
              path: `properties.${kind}[${i}]`,
              message: "must be a mapping",
            });
            continue;
          }
          const key = e.key;
          if (typeof key !== "string" || !PROP_KEY_RE.test(key)) {
            errors.push({
              path: `properties.${kind}[${i}].key`,
              message: `key must match ${PROP_KEY_RE}`,
            });
          }
          if (e.values !== undefined) {
            if (!Array.isArray(e.values)) {
              errors.push({
                path: `properties.${kind}[${i}].values`,
                message: "values must be an array when present",
              });
            } else {
              const seen = new Set<string>();
              for (let j = 0; j < e.values.length; j++) {
                if (typeof e.values[j] !== "string") {
                  errors.push({
                    path: `properties.${kind}[${i}].values[${j}]`,
                    message: "each value must be a string",
                  });
                } else {
                  const v = e.values[j] as string;
                  if (seen.has(v)) {
                    errors.push({
                      path: `properties.${kind}[${i}].values[${j}]`,
                      message: `Property ${key}: duplicate value '${v}' in values list`,
                    });
                  } else {
                    seen.add(v);
                  }
                }
              }
            }
          }
        }
      }
    }
  }

  const valid = errors.length === 0;
  return {
    valid,
    manifest: valid ? (obj as unknown as PackManifest) : null,
    errors,
    warnings,
  };
}

/**
 * Read and validate a pack manifest from disk. Returns a PackValidationResult.
 * `manifestPath` should be a path to a `pack.yaml` file, OR a directory
 * containing one (we'll append `/pack.yaml` if so).
 */
export async function loadAndValidatePack(
  manifestPath: string,
): Promise<PackValidationResult> {
  let path = manifestPath;
  let text: string;
  try {
    const stat = await Deno.stat(path);
    if (stat.isDirectory) path = `${path.replace(/\/$/, "")}/pack.yaml`;
    text = await Deno.readTextFile(path);
  } catch (err) {
    return {
      valid: false,
      manifest: null,
      errors: [{
        path: "",
        message: `cannot read '${path}': ${(err as Error).message}`,
      }],
      warnings: [],
    };
  }

  let parsed: unknown;
  try {
    parsed = parseYaml(text);
  } catch (err) {
    return {
      valid: false,
      manifest: null,
      errors: [{
        path: "",
        message: `YAML parse error in ${path}: ${(err as Error).message}`,
      }],
      warnings: [],
    };
  }
  return validatePackManifest(parsed);
}

// ─── Closed sets exposed for tests / callers ─────────────────────────────────

export const _BASE_ENTITY_KINDS = BASE_ENTITY_KINDS;
export const _BASE_EDGE_RELATIONS = BASE_EDGE_RELATIONS;
