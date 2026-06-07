/**
 * Tests for `khive kg stats`.
 */

import { assertEquals } from "@std/assert";
import { computeStats } from "./stats.ts";

// ─── Test harness ─────────────────────────────────────────────────────────────

async function makeTempDir(): Promise<string> {
  return await Deno.makeTempDir({ prefix: "khive_stats_test_" });
}

async function removeDir(path: string): Promise<void> {
  await Deno.remove(path, { recursive: true });
}

async function setupKg(
  dir: string,
  entities: unknown[],
  edges: unknown[],
): Promise<void> {
  await Deno.mkdir(`${dir}/.khive/kg`, { recursive: true });
  const entitiesNdjson = entities.map((e) => JSON.stringify(e)).join("\n") +
    (entities.length > 0 ? "\n" : "");
  const edgesNdjson = edges.map((e) => JSON.stringify(e)).join("\n") +
    (edges.length > 0 ? "\n" : "");
  await Deno.writeTextFile(`${dir}/.khive/kg/entities.ndjson`, entitiesNdjson);
  await Deno.writeTextFile(`${dir}/.khive/kg/edges.ndjson`, edgesNdjson);
}

// ─── Fixtures ─────────────────────────────────────────────────────────────────

const ID_A = "aaaaaaaa-0000-0000-0000-000000000001";
const ID_B = "bbbbbbbb-0000-0000-0000-000000000002";
const ID_C = "cccccccc-0000-0000-0000-000000000003";

const ENTITY_A = { id: ID_A, kind: "concept", name: "A" };
const ENTITY_B = { id: ID_B, kind: "project", name: "B" };
const ENTITY_C = { id: ID_C, kind: "concept", name: "C" };

const EDGE_AB = {
  edge_id: "eeeeeeee-0000-0000-0000-000000000001",
  source: ID_B,
  target: ID_A,
  relation: "depends_on",
};
const EDGE_CB = {
  edge_id: "eeeeeeee-0000-0000-0000-000000000002",
  source: ID_C,
  target: ID_B,
  relation: "implements",
};

// ─── Tests ────────────────────────────────────────────────────────────────────

Deno.test("computeStats: empty KG returns all zeros", async () => {
  const dir = await makeTempDir();
  try {
    await setupKg(dir, [], []);
    const stats = await computeStats(dir);
    assertEquals(stats.entityCount, 0);
    assertEquals(stats.edgeCount, 0);
    assertEquals(stats.orphanEntityCount, 0);
    assertEquals(Object.keys(stats.entityKinds).length, 0);
    assertEquals(Object.keys(stats.edgeRelations).length, 0);
    assertEquals(stats.schemaCoverage.entityKindsKnown, 0);
    assertEquals(stats.schemaCoverage.entityKindsUnknown, 0);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeStats: counts entities correctly", async () => {
  const dir = await makeTempDir();
  try {
    await setupKg(dir, [ENTITY_A, ENTITY_B, ENTITY_C], []);
    const stats = await computeStats(dir);
    assertEquals(stats.entityCount, 3);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeStats: counts edges correctly", async () => {
  const dir = await makeTempDir();
  try {
    await setupKg(dir, [ENTITY_A, ENTITY_B, ENTITY_C], [EDGE_AB, EDGE_CB]);
    const stats = await computeStats(dir);
    assertEquals(stats.edgeCount, 2);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeStats: entity kind breakdown", async () => {
  const dir = await makeTempDir();
  try {
    await setupKg(dir, [ENTITY_A, ENTITY_B, ENTITY_C], []);
    const stats = await computeStats(dir);
    assertEquals(stats.entityKinds["concept"], 2); // A and C
    assertEquals(stats.entityKinds["project"], 1); // B
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeStats: edge relation breakdown", async () => {
  const dir = await makeTempDir();
  try {
    await setupKg(dir, [ENTITY_A, ENTITY_B, ENTITY_C], [EDGE_AB, EDGE_CB]);
    const stats = await computeStats(dir);
    assertEquals(stats.edgeRelations["depends_on"], 1);
    assertEquals(stats.edgeRelations["implements"], 1);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeStats: orphan entity not referenced by any edge", async () => {
  const dir = await makeTempDir();
  try {
    // ENTITY_A only — no edges
    await setupKg(dir, [ENTITY_A], []);
    const stats = await computeStats(dir);
    assertEquals(stats.orphanEntityCount, 1);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeStats: no orphans when all entities are referenced", async () => {
  const dir = await makeTempDir();
  try {
    await setupKg(dir, [ENTITY_A, ENTITY_B], [EDGE_AB]);
    const stats = await computeStats(dir);
    assertEquals(stats.orphanEntityCount, 0);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeStats: partially referenced entities counted as orphans", async () => {
  const dir = await makeTempDir();
  try {
    // EDGE_AB references A and B, but C is unreferenced
    await setupKg(dir, [ENTITY_A, ENTITY_B, ENTITY_C], [EDGE_AB]);
    const stats = await computeStats(dir);
    assertEquals(stats.orphanEntityCount, 1); // C is orphan
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeStats: schema coverage known kinds", async () => {
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(`${dir}/.khive/kg`, { recursive: true });
    await Deno.writeTextFile(
      `${dir}/.khive/kg/schema.yaml`,
      `format_version: "1.0.0"\nentity_kinds:\n  - concept\n  - project\nedge_relations:\n  - relation: depends_on\n  - relation: implements\n`,
    );
    await setupKg(dir, [ENTITY_A, ENTITY_B], [EDGE_AB]);
    const stats = await computeStats(dir);
    assertEquals(stats.schemaCoverage.entityKindsKnown, 2);
    assertEquals(stats.schemaCoverage.entityKindsUnknown, 0);
    assertEquals(stats.schemaCoverage.edgeRelationsKnown, 1);
    assertEquals(stats.schemaCoverage.edgeRelationsUnknown, 0);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeStats: schema coverage unknown kinds", async () => {
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(`${dir}/.khive/kg`, { recursive: true });
    await Deno.writeTextFile(
      `${dir}/.khive/kg/schema.yaml`,
      `format_version: "1.0.0"\nentity_kinds:\n  - concept\nedge_relations:\n  - relation: contains\n`,
    );
    // ENTITY_B has kind "project" (unknown), EDGE_AB has relation "depends_on" (unknown)
    await setupKg(dir, [ENTITY_A, ENTITY_B], [EDGE_AB]);
    const stats = await computeStats(dir);
    assertEquals(stats.schemaCoverage.entityKindsKnown, 1); // concept
    assertEquals(stats.schemaCoverage.entityKindsUnknown, 1); // project
    assertEquals(stats.schemaCoverage.edgeRelationsKnown, 0); // depends_on not in schema
    assertEquals(stats.schemaCoverage.edgeRelationsUnknown, 1);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeStats: missing NDJSON files treated as empty", async () => {
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(`${dir}/.khive/kg`, { recursive: true });
    // No NDJSON files — readNdjson handles NotFound gracefully
    const stats = await computeStats(dir);
    assertEquals(stats.entityCount, 0);
    assertEquals(stats.edgeCount, 0);
    assertEquals(stats.orphanEntityCount, 0);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeStats: invalid JSON lines in NDJSON are skipped", async () => {
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(`${dir}/.khive/kg`, { recursive: true });
    // Mix of valid and invalid lines
    await Deno.writeTextFile(
      `${dir}/.khive/kg/entities.ndjson`,
      `not-json\n${JSON.stringify(ENTITY_A)}\nbad-line\n`,
    );
    await Deno.writeTextFile(`${dir}/.khive/kg/edges.ndjson`, "");
    const stats = await computeStats(dir);
    // Only the valid entity counts
    assertEquals(stats.entityCount, 1);
  } finally {
    await removeDir(dir);
  }
});
