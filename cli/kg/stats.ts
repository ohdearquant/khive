/**
 * `khive kg stats` — show KG statistics.
 *
 * Counts entities, edges, orphan nodes, and schema coverage.
 */

import { readNdjson } from "../lib/ndjson.ts";
import { EDGES_FILE, ENTITIES_FILE, KG_DIR } from "../lib/paths.ts";
import { loadSchema } from "../lib/schema.ts";

// ─── Types ────────────────────────────────────────────────────────────────────

interface KgStats {
  entityCount: number;
  edgeCount: number;
  orphanEntityCount: number;
  entityKinds: Record<string, number>;
  edgeRelations: Record<string, number>;
  schemaCoverage: {
    entityKindsKnown: number;
    entityKindsUnknown: number;
    edgeRelationsKnown: number;
    edgeRelationsUnknown: number;
  };
}

// ─── Core logic ───────────────────────────────────────────────────────────────

export async function computeStats(repoRoot: string): Promise<KgStats> {
  let schemaEntityKinds = new Set<string>();
  let schemaEdgeRelations = new Set<string>();
  try {
    const schema = await loadSchema(repoRoot);
    schemaEntityKinds = new Set(schema.entity_kinds);
    schemaEdgeRelations = new Set(schema.edge_relations.map((r) => r.relation));
  } catch {
    // No schema — coverage counts will be 0 known.
  }

  // Pass 1: read entities
  const entityIds = new Set<string>();
  const entityKinds: Record<string, number> = {};

  for await (const entry of readNdjson(`${repoRoot}/${ENTITIES_FILE}`)) {
    if (!entry.data) continue;
    const id = entry.data["id"];
    if (typeof id === "string" && id.length > 0) entityIds.add(id);
    const kind = typeof entry.data["kind"] === "string" ? entry.data["kind"] : "unknown";
    entityKinds[kind] = (entityKinds[kind] ?? 0) + 1;
  }

  // Pass 2: read edges, track referenced entity IDs for orphan detection
  const referencedIds = new Set<string>();
  const edgeRelations: Record<string, number> = {};
  let edgeCount = 0;

  for await (const entry of readNdjson(`${repoRoot}/${EDGES_FILE}`)) {
    if (!entry.data) continue;
    edgeCount++;
    const relation = typeof entry.data["relation"] === "string"
      ? entry.data["relation"]
      : "unknown";
    edgeRelations[relation] = (edgeRelations[relation] ?? 0) + 1;
    const source = entry.data["source"];
    const target = entry.data["target"];
    if (typeof source === "string" && source.length > 0) referencedIds.add(source);
    if (typeof target === "string" && target.length > 0) referencedIds.add(target);
  }

  // Orphan = entity not referenced by any edge endpoint
  let orphanEntityCount = 0;
  for (const id of entityIds) {
    if (!referencedIds.has(id)) orphanEntityCount++;
  }

  // Schema coverage: how many present kinds/relations are in the schema
  const presentEntityKinds = new Set(Object.keys(entityKinds));
  const presentEdgeRelations = new Set(Object.keys(edgeRelations));

  const entityKindsKnown = [...presentEntityKinds].filter((k) => schemaEntityKinds.has(k)).length;
  const entityKindsUnknown =
    [...presentEntityKinds].filter((k) => !schemaEntityKinds.has(k)).length;
  const edgeRelationsKnown =
    [...presentEdgeRelations].filter((r) => schemaEdgeRelations.has(r)).length;
  const edgeRelationsUnknown =
    [...presentEdgeRelations].filter((r) => !schemaEdgeRelations.has(r)).length;

  return {
    entityCount: entityIds.size,
    edgeCount,
    orphanEntityCount,
    entityKinds,
    edgeRelations,
    schemaCoverage: {
      entityKindsKnown,
      entityKindsUnknown,
      edgeRelationsKnown,
      edgeRelationsUnknown,
    },
  };
}

// ─── Formatting ───────────────────────────────────────────────────────────────

function formatStats(stats: KgStats, json: boolean): string {
  if (json) return JSON.stringify(stats, null, 2);

  const lines: string[] = ["KG Statistics"];
  lines.push(`  Entities:        ${stats.entityCount}`);
  lines.push(`  Edges:           ${stats.edgeCount}`);
  lines.push(`  Orphan entities: ${stats.orphanEntityCount}`);

  if (Object.keys(stats.entityKinds).length > 0) {
    lines.push("\n  Entity kinds:");
    for (const [kind, count] of Object.entries(stats.entityKinds).sort()) {
      lines.push(`    ${kind}: ${count}`);
    }
  }

  if (Object.keys(stats.edgeRelations).length > 0) {
    lines.push("\n  Edge relations:");
    for (const [rel, count] of Object.entries(stats.edgeRelations).sort()) {
      lines.push(`    ${rel}: ${count}`);
    }
  }

  const cov = stats.schemaCoverage;
  lines.push(`\n  Schema coverage:`);
  lines.push(
    `    Entity kinds:   ${cov.entityKindsKnown} known, ${cov.entityKindsUnknown} unknown`,
  );
  lines.push(
    `    Edge relations: ${cov.edgeRelationsKnown} known, ${cov.edgeRelationsUnknown} unknown`,
  );

  return lines.join("\n");
}

// ─── CLI entry point ──────────────────────────────────────────────────────────

export async function runStats(repoRoot: string, args: string[]): Promise<void> {
  if (args.includes("--help") || args.includes("-h")) {
    console.log(`Usage: khive kg stats [--json]

Show KG statistics: entity/edge counts, kind/relation breakdown, schema coverage.

Flags:
  --json    Output statistics as JSON`);
    return;
  }

  try {
    await Deno.stat(`${repoRoot}/${KG_DIR}`);
  } catch {
    console.log("KG not initialized. Run 'khive kg init' to start.");
    return;
  }

  const json = args.includes("--json");
  console.log(formatStats(await computeStats(repoRoot), json));
}
