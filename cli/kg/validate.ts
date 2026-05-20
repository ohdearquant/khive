/**
 * NDJSON + schema.yaml validation for `khive kg` commands (ADR-048, ADR-051).
 *
 * Used by `khive kg commit` and `khive kg sync`.
 * Can also be invoked directly: `khive kg validate`.
 */

import { EDGES_FILE, ENTITIES_FILE, SCHEMA_FILE } from "../lib/paths.ts";
import {
  EDGE_RELATIONS,
  ENTITY_KINDS,
  parseEdgeLine,
  parseEntityLine,
  readNdjson,
} from "../lib/ndjson.ts";
import { loadSchema, validateSchema } from "../lib/schema.ts";

// ─── Result types ─────────────────────────────────────────────────────────────

export interface ValidationError {
  file: string;
  line: number;
  message: string;
}

export interface ValidationWarning {
  file: string;
  line: number;
  message: string;
}

export interface ValidationResult {
  valid: boolean;
  errors: ValidationError[];
  warnings: ValidationWarning[];
  entityCount: number;
  edgeCount: number;
}

// ─── Validator ────────────────────────────────────────────────────────────────

/**
 * Validate entities.ndjson, edges.ndjson, and schema.yaml under repoRoot.
 *
 * Checks:
 *   1. Each entity line parses as JSON with id/name/kind fields.
 *   2. Entity kind is in the closed set.
 *   3. Entity kind is declared in schema.yaml entity_kinds (error, not warning).
 *   4. Duplicate entity id check.
 *   5. Entity sort order: UUID-ascending.
 *   6. Each edge line parses as JSON with edge_id/source/target/relation (ADR-048 field names).
 *   7. Edge relation is in the closed set.
 *   8. Edge relation is declared in schema.yaml edge_relations (error, not warning).
 *   9. Referential integrity: source and target must be known entity IDs (skip remote refs).
 *  10. Composite key duplicate check: (source, target, relation) must be unique.
 *  11. Edge sort order: composite-key-ascending (source + target + relation).
 *  12. schema.yaml structural validity.
 */
export async function validate(repoRoot: string): Promise<ValidationResult> {
  const errors: ValidationError[] = [];
  const warnings: ValidationWarning[] = [];
  let entityCount = 0;
  let edgeCount = 0;

  // ── 1. Load schema ────────────────────────────────────────────────────────
  let schemaEntityKinds: Set<string> = new Set(ENTITY_KINDS);
  let schemaRelations: Set<string> = new Set(EDGE_RELATIONS);
  let schemaRemotes: Set<string> = new Set();

  try {
    const schema = await loadSchema(repoRoot);
    const schemaErrors = validateSchema(schema);
    for (const e of schemaErrors) {
      errors.push(e);
    }
    if (schema.entity_kinds.length > 0) {
      schemaEntityKinds = new Set(schema.entity_kinds);
    }
    if (schema.edge_relations.length > 0) {
      schemaRelations = new Set(schema.edge_relations.map((r) => r.relation));
    }
    if (schema.remotes && schema.remotes.length > 0) {
      for (const r of schema.remotes) {
        if (!r.name || !r.repo || !r.path || !r.commit) {
          errors.push({
            file: SCHEMA_FILE,
            line: 0,
            message: `Remote '${
              r.name || "(unnamed)"
            }' missing required fields (name, repo, path, commit)`,
          });
        } else if (!/^[0-9a-f]{40}$/i.test(r.commit)) {
          errors.push({
            file: SCHEMA_FILE,
            line: 0,
            message: `Remote '${r.name}' commit must be a 40-character SHA, got '${r.commit}'`,
          });
        }
        if (r.name) schemaRemotes.add(r.name);
      }
    }
  } catch (err) {
    errors.push({
      file: SCHEMA_FILE,
      line: 0,
      message: `Cannot load schema.yaml: ${(err as Error).message}`,
    });
  }

  // ── 2. Validate entities.ndjson ───────────────────────────────────────────
  const entitiesPath = `${repoRoot}/${ENTITIES_FILE}`;
  const seenEntityIds = new Set<string>();
  let prevEntityId = "";

  try {
    for await (const entry of readNdjson(entitiesPath)) {
      entityCount++;

      if (entry.data === null) {
        errors.push({ file: ENTITIES_FILE, line: entry.line, message: entry.error! });
        continue;
      }
      const { line, data } = entry;

      const entity = parseEntityLine(data);
      if (!entity) {
        errors.push({
          file: ENTITIES_FILE,
          line,
          message: `Invalid entity: must have id (UUID), name (string), kind (one of: ${
            ENTITY_KINDS.join(", ")
          })`,
        });
        continue;
      }

      // Cross-check kind against schema (error, not warning — codex finding)
      if (!schemaEntityKinds.has(entity.kind)) {
        errors.push({
          file: ENTITIES_FILE,
          line,
          message: `Entity kind '${entity.kind}' not declared in schema.yaml entity_kinds`,
        });
      }

      // Duplicate ID check
      if (seenEntityIds.has(entity.id)) {
        errors.push({
          file: ENTITIES_FILE,
          line,
          message: `Duplicate entity id: ${entity.id}`,
        });
      }
      seenEntityIds.add(entity.id);

      // Sort order: UUID-ascending
      if (prevEntityId !== "" && entity.id < prevEntityId) {
        errors.push({
          file: ENTITIES_FILE,
          line,
          message:
            `Entity out of sort order: '${entity.id}' must come after '${prevEntityId}' (entities.ndjson must be UUID-ascending)`,
        });
      }
      prevEntityId = entity.id;
    }
  } catch (err) {
    if (!(err instanceof Deno.errors.NotFound)) {
      errors.push({
        file: ENTITIES_FILE,
        line: 0,
        message: `Cannot read entities.ndjson: ${(err as Error).message}`,
      });
    }
  }

  // ── 3. Validate edges.ndjson ──────────────────────────────────────────────
  const edgesPath = `${repoRoot}/${EDGES_FILE}`;
  const seenEdgeIds = new Set<string>();
  const seenCompositeKeys = new Set<string>();
  let prevCompositeKey = "";

  try {
    for await (const entry of readNdjson(edgesPath)) {
      edgeCount++;

      if (entry.data === null) {
        errors.push({ file: EDGES_FILE, line: entry.line, message: entry.error! });
        continue;
      }
      const { line, data } = entry;

      const edge = parseEdgeLine(data);
      if (!edge) {
        errors.push({
          file: EDGES_FILE,
          line,
          message:
            `Invalid edge: must have edge_id (UUID), source (string), target (string), relation (one of: ${
              EDGE_RELATIONS.join(", ")
            })`,
        });
        continue;
      }

      // Cross-check relation against schema (error, not warning — codex finding)
      if (!schemaRelations.has(edge.relation)) {
        errors.push({
          file: EDGES_FILE,
          line,
          message: `Edge relation '${edge.relation}' not declared in schema.yaml edge_relations`,
        });
      }

      // Referential integrity: source MUST be a local UUID. target may be a
      // remote reference in `<remote>:<uuid>` format (ADR-048 §5).
      const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
      const REMOTE_REF_RE =
        /^([a-z][a-z0-9_-]*):([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})$/i;

      if (!UUID_RE.test(edge.source)) {
        errors.push({
          file: EDGES_FILE,
          line,
          message: `Edge source must be a local UUID, got '${edge.source}'`,
        });
      } else if (!seenEntityIds.has(edge.source)) {
        errors.push({
          file: EDGES_FILE,
          line,
          message: `Edge source '${edge.source}' does not reference a known entity id`,
        });
      }

      if (UUID_RE.test(edge.target)) {
        if (!seenEntityIds.has(edge.target)) {
          errors.push({
            file: EDGES_FILE,
            line,
            message: `Edge target '${edge.target}' does not reference a known entity id`,
          });
        }
      } else {
        const remoteMatch = REMOTE_REF_RE.exec(edge.target);
        if (!remoteMatch) {
          errors.push({
            file: EDGES_FILE,
            line,
            message:
              `Edge target '${edge.target}' is neither a UUID nor a valid remote ref (<remote>:<uuid>)`,
          });
        } else {
          const remoteName = remoteMatch[1];
          if (!schemaRemotes.has(remoteName)) {
            errors.push({
              file: EDGES_FILE,
              line,
              message:
                `Edge target references undeclared remote '${remoteName}' (not in schema.yaml#remotes)`,
            });
          }
        }
      }

      // Duplicate edge_id check
      if (seenEdgeIds.has(edge.edge_id)) {
        errors.push({
          file: EDGES_FILE,
          line,
          message: `Duplicate edge_id: ${edge.edge_id}`,
        });
      }
      seenEdgeIds.add(edge.edge_id);

      // Composite key duplicate check: (source, target, relation) must be unique
      const compositeKey = `${edge.source}\x00${edge.target}\x00${edge.relation}`;
      if (seenCompositeKeys.has(compositeKey)) {
        errors.push({
          file: EDGES_FILE,
          line,
          message:
            `Duplicate edge composite key (source, target, relation): (${edge.source}, ${edge.target}, ${edge.relation})`,
        });
      }
      seenCompositeKeys.add(compositeKey);

      // Sort order: composite-key-ascending (source + target + relation)
      if (prevCompositeKey !== "" && compositeKey < prevCompositeKey) {
        errors.push({
          file: EDGES_FILE,
          line,
          message:
            `Edge out of sort order at composite key '${edge.source}+${edge.target}+${edge.relation}' (edges.ndjson must be composite-key-ascending)`,
        });
      }
      prevCompositeKey = compositeKey;
    }
  } catch (err) {
    if (!(err instanceof Deno.errors.NotFound)) {
      errors.push({
        file: EDGES_FILE,
        line: 0,
        message: `Cannot read edges.ndjson: ${(err as Error).message}`,
      });
    }
  }

  return {
    valid: errors.length === 0,
    errors,
    warnings,
    entityCount,
    edgeCount,
  };
}

// ─── CLI entry point ──────────────────────────────────────────────────────────

/**
 * Print a ValidationResult to stdout in a human-readable format.
 */
export function printValidationResult(result: ValidationResult): void {
  if (result.valid) {
    console.log(
      `Validation: pass (${result.entityCount} entities, ${result.edgeCount} edges)`,
    );
  } else {
    console.error(`Validation: fail — ${result.errors.length} error(s)`);
  }

  if (result.errors.length > 0) {
    const shown = result.errors.slice(0, 5);
    for (const e of shown) {
      console.error(`  ERROR  ${e.file}:${e.line}  ${e.message}`);
    }
    if (result.errors.length > 5) {
      console.error(`  ... and ${result.errors.length - 5} more error(s)`);
    }
  }

  if (result.warnings.length > 0) {
    for (const w of result.warnings) {
      console.warn(`  WARN   ${w.file}:${w.line}  ${w.message}`);
    }
  }
}

/**
 * `khive kg validate` command.
 *
 * Args: none (reads from current repo root).
 * Exits 0 on pass, 1 on validation failure.
 */
export async function runValidate(repoRoot: string): Promise<void> {
  const result = await validate(repoRoot);
  printValidationResult(result);
  if (!result.valid) {
    Deno.exit(1);
  }
}
