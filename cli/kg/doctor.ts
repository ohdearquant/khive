/**
 * `khive kg doctor` — validate KG integrity.
 *
 * Checks:
 *   Errors (cause non-zero exit):
 *     INVALID_JSON        — a line fails JSON.parse
 *     MISSING_FIELD       — required field absent
 *     DUPLICATE_ID        — two entities share the same id
 *     DUPLICATE_EDGE_ID   — two edges share the same edge_id
 *     DUPLICATE_NATURAL_KEY — two edges share (source, target, relation)
 *     DANGLING_REF        — edge references a non-existent entity
 *
 *   Warnings (informational, do not fail):
 *     SCHEMA_MISSING      — schema.yaml absent; kind/relation checks skipped
 *     UNKNOWN_KIND        — entity kind not in schema
 *     UNKNOWN_RELATION    — edge relation not in schema
 *     ORPHAN_ENTITY       — entity unreferenced by any edge
 */

import { readNdjson } from "../lib/ndjson.ts";
import { EDGES_FILE, ENTITIES_FILE, KG_DIR } from "../lib/paths.ts";
import { loadSchema } from "../lib/schema.ts";

// ─── Types ────────────────────────────────────────────────────────────────────

type DoctorSeverity = "error" | "warning";

interface DoctorIssue {
  severity: DoctorSeverity;
  code: string;
  file: string;
  line?: number;
  message: string;
}

interface DoctorReport {
  valid: boolean;
  issues: DoctorIssue[];
}

// ─── Core logic ───────────────────────────────────────────────────────────────

export async function inspectKg(repoRoot: string): Promise<DoctorReport> {
  const issues: DoctorIssue[] = [];

  let schemaEntityKinds = new Set<string>();
  let schemaEdgeRelations = new Set<string>();
  let schemaLoaded = false;
  try {
    const schema = await loadSchema(repoRoot);
    schemaEntityKinds = new Set(schema.entity_kinds);
    schemaEdgeRelations = new Set(schema.edge_relations.map((r) => r.relation));
    schemaLoaded = true;
  } catch {
    issues.push({
      severity: "warning",
      code: "SCHEMA_MISSING",
      file: ".khive/kg/schema.yaml",
      message: "schema.yaml not found or invalid — kind/relation checks skipped",
    });
  }

  // ── Pass 1: validate entities ────────────────────────────────────────────
  const entityIds = new Set<string>();
  const entityFirstLine = new Map<string, number>();

  for await (const entry of readNdjson(`${repoRoot}/${ENTITIES_FILE}`)) {
    if (entry.data === null) {
      issues.push({
        severity: "error",
        code: "INVALID_JSON",
        file: ENTITIES_FILE,
        line: entry.line,
        message: entry.error,
      });
      continue;
    }

    const data = entry.data;
    const id = data["id"];
    const name = data["name"];
    const kind = data["kind"];

    if (typeof id !== "string" || id.length === 0) {
      issues.push({
        severity: "error",
        code: "MISSING_FIELD",
        file: ENTITIES_FILE,
        line: entry.line,
        message: `Entity missing required 'id' field`,
      });
      continue;
    }

    if (entityIds.has(id)) {
      issues.push({
        severity: "error",
        code: "DUPLICATE_ID",
        file: ENTITIES_FILE,
        line: entry.line,
        message: `Duplicate entity id '${id}' (first seen on line ${entityFirstLine.get(id)})`,
      });
    } else {
      entityIds.add(id);
      entityFirstLine.set(id, entry.line);
    }

    if (typeof name !== "string" || name.length === 0) {
      issues.push({
        severity: "error",
        code: "MISSING_FIELD",
        file: ENTITIES_FILE,
        line: entry.line,
        message: `Entity '${id}' missing required 'name' field`,
      });
    }

    if (typeof kind !== "string" || kind.length === 0) {
      issues.push({
        severity: "error",
        code: "MISSING_FIELD",
        file: ENTITIES_FILE,
        line: entry.line,
        message: `Entity '${id}' missing required 'kind' field`,
      });
    } else if (schemaLoaded && !schemaEntityKinds.has(kind)) {
      issues.push({
        severity: "warning",
        code: "UNKNOWN_KIND",
        file: ENTITIES_FILE,
        line: entry.line,
        message: `Entity '${id}' has unknown kind '${kind}'`,
      });
    }
  }

  // ── Pass 2: validate edges ────────────────────────────────────────────────
  const edgeIds = new Set<string>();
  const naturalKeys = new Set<string>(); // source + "\x00" + target + "\x00" + relation
  const referencedEntityIds = new Set<string>();

  for await (const entry of readNdjson(`${repoRoot}/${EDGES_FILE}`)) {
    if (entry.data === null) {
      issues.push({
        severity: "error",
        code: "INVALID_JSON",
        file: EDGES_FILE,
        line: entry.line,
        message: entry.error,
      });
      continue;
    }

    const data = entry.data;
    const edgeId = data["edge_id"];
    const source = data["source"];
    const target = data["target"];
    const relation = data["relation"];
    const edgeLabel = typeof edgeId === "string" ? edgeId : "(unknown)";

    if (typeof edgeId !== "string" || edgeId.length === 0) {
      issues.push({
        severity: "error",
        code: "MISSING_FIELD",
        file: EDGES_FILE,
        line: entry.line,
        message: `Edge missing required 'edge_id' field`,
      });
    } else {
      if (edgeIds.has(edgeId)) {
        issues.push({
          severity: "error",
          code: "DUPLICATE_EDGE_ID",
          file: EDGES_FILE,
          line: entry.line,
          message: `Duplicate edge_id '${edgeId}'`,
        });
      } else {
        edgeIds.add(edgeId);
      }
    }

    if (typeof source !== "string" || source.length === 0) {
      issues.push({
        severity: "error",
        code: "MISSING_FIELD",
        file: EDGES_FILE,
        line: entry.line,
        message: `Edge '${edgeLabel}' missing required 'source' field`,
      });
    } else {
      referencedEntityIds.add(source);
    }

    if (typeof target !== "string" || target.length === 0) {
      issues.push({
        severity: "error",
        code: "MISSING_FIELD",
        file: EDGES_FILE,
        line: entry.line,
        message: `Edge '${edgeLabel}' missing required 'target' field`,
      });
    } else {
      referencedEntityIds.add(target);
    }

    if (typeof relation !== "string" || relation.length === 0) {
      issues.push({
        severity: "error",
        code: "MISSING_FIELD",
        file: EDGES_FILE,
        line: entry.line,
        message: `Edge '${edgeLabel}' missing required 'relation' field`,
      });
    } else {
      if (schemaLoaded && !schemaEdgeRelations.has(relation)) {
        issues.push({
          severity: "warning",
          code: "UNKNOWN_RELATION",
          file: EDGES_FILE,
          line: entry.line,
          message: `Edge '${edgeLabel}' has unknown relation '${relation}'`,
        });
      }
    }

    if (
      typeof source === "string" &&
      typeof target === "string" &&
      typeof relation === "string"
    ) {
      const naturalKey = `${source}\x00${target}\x00${relation}`;
      if (naturalKeys.has(naturalKey)) {
        issues.push({
          severity: "error",
          code: "DUPLICATE_NATURAL_KEY",
          file: EDGES_FILE,
          line: entry.line,
          message: `Duplicate edge (source=${source}, target=${target}, relation=${relation})`,
        });
      } else {
        naturalKeys.add(naturalKey);
      }
    }
  }

  // ── Pass 3: referential integrity ──────────────────────────────────────────
  for (const refId of referencedEntityIds) {
    if (!entityIds.has(refId)) {
      issues.push({
        severity: "error",
        code: "DANGLING_REF",
        file: EDGES_FILE,
        message: `Edge references non-existent entity '${refId}'`,
      });
    }
  }

  // ── Pass 4: orphan entities (warning only) ─────────────────────────────────
  for (const entityId of entityIds) {
    if (!referencedEntityIds.has(entityId)) {
      issues.push({
        severity: "warning",
        code: "ORPHAN_ENTITY",
        file: ENTITIES_FILE,
        message: `Entity '${entityId}' is not referenced by any edge`,
      });
    }
  }

  const hasErrors = issues.some((i) => i.severity === "error");

  return { valid: !hasErrors, issues };
}

// ─── Formatting ───────────────────────────────────────────────────────────────

function formatDoctor(report: DoctorReport, json: boolean): string {
  if (json) return JSON.stringify(report, null, 2);

  if (report.issues.length === 0) {
    return "KG doctor: all checks passed.";
  }

  const errors = report.issues.filter((i) => i.severity === "error");
  const warnings = report.issues.filter((i) => i.severity === "warning");

  const lines: string[] = [
    `KG doctor: ${errors.length} error(s), ${warnings.length} warning(s)`,
  ];

  for (const issue of report.issues) {
    const loc = issue.line !== undefined ? `${issue.file}:${issue.line}` : issue.file;
    const prefix = issue.severity === "error" ? "ERROR" : "WARN ";
    lines.push(`  [${prefix}] ${issue.code}: ${issue.message} (${loc})`);
  }

  return lines.join("\n");
}

// ─── CLI entry point ──────────────────────────────────────────────────────────

export async function runDoctor(repoRoot: string, args: string[]): Promise<void> {
  if (args.includes("--help") || args.includes("-h")) {
    console.log(`Usage: khive kg doctor [--json]

Validate KG integrity: NDJSON syntax, referential integrity, duplicate detection.

Flags:
  --json    Output report as JSON

Exits 1 if any errors are found. Warnings alone do not cause non-zero exit.`);
    return;
  }

  try {
    await Deno.stat(`${repoRoot}/${KG_DIR}`);
  } catch {
    console.log("KG not initialized. Run 'khive kg init' to start.");
    return;
  }

  const json = args.includes("--json");
  const report = await inspectKg(repoRoot);
  console.log(formatDoctor(report, json));
  if (!report.valid) Deno.exit(1);
}
