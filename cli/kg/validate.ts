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
 *   3. Each edge line parses as JSON with id/source_id/target_id/relation.
 *   4. Edge relation is in the closed set.
 *   5. schema.yaml structural validity.
 *   6. Entity kinds in data are a subset of schema.yaml entity_kinds.
 *   7. Edge relations in data are a subset of schema.yaml edge_relations.
 */
export async function validate(repoRoot: string): Promise<ValidationResult> {
  const errors: ValidationError[] = [];
  const warnings: ValidationWarning[] = [];
  let entityCount = 0;
  let edgeCount = 0;

  // ── 1. Load schema ────────────────────────────────────────────────────────
  let schemaEntityKinds: Set<string> = new Set(ENTITY_KINDS);
  let schemaRelations: Set<string> = new Set(EDGE_RELATIONS);

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
  } catch (err) {
    errors.push({
      file: SCHEMA_FILE,
      line: 0,
      message: `Cannot load schema.yaml: ${(err as Error).message}`,
    });
    // Proceed with default closed sets so we can still validate NDJSON files
  }

  // ── 2. Validate entities.ndjson ───────────────────────────────────────────
  const entitiesPath = `${repoRoot}/${ENTITIES_FILE}`;
  const seenEntityIds = new Set<string>();

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

      // Cross-check kind against schema
      if (!schemaEntityKinds.has(entity.kind)) {
        warnings.push({
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
            `Invalid edge: must have id (string), source_id (string), target_id (string), relation (one of: ${
              EDGE_RELATIONS.join(", ")
            })`,
        });
        continue;
      }

      // Cross-check relation against schema
      if (!schemaRelations.has(edge.relation)) {
        warnings.push({
          file: EDGES_FILE,
          line,
          message: `Edge relation '${edge.relation}' not declared in schema.yaml edge_relations`,
        });
      }

      // Duplicate ID check
      if (seenEdgeIds.has(edge.id)) {
        errors.push({
          file: EDGES_FILE,
          line,
          message: `Duplicate edge id: ${edge.id}`,
        });
      }
      seenEdgeIds.add(edge.id);
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
