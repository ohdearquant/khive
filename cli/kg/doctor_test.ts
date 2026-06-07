/**
 * Tests for `khive kg doctor`.
 */

import { assertEquals } from "@std/assert";
import { inspectKg } from "./doctor.ts";

// ─── Test harness ─────────────────────────────────────────────────────────────

async function makeTempDir(): Promise<string> {
  return await Deno.makeTempDir({ prefix: "khive_doctor_test_" });
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
const EDGE_ID_1 = "eeeeeeee-0000-0000-0000-000000000001";
const EDGE_ID_2 = "ffffffff-0000-0000-0000-000000000002";

const ENTITY_A = { id: ID_A, kind: "concept", name: "A" };
const ENTITY_B = { id: ID_B, kind: "project", name: "B" };

const EDGE_AB = {
  edge_id: EDGE_ID_1,
  source: ID_B,
  target: ID_A,
  relation: "depends_on",
};

// ─── Valid KG ─────────────────────────────────────────────────────────────────

Deno.test("doctor: valid KG produces no errors", async () => {
  const dir = await makeTempDir();
  try {
    await setupKg(dir, [ENTITY_A, ENTITY_B], [EDGE_AB]);
    const report = await inspectKg(dir);
    const errors = report.issues.filter((i) => i.severity === "error");
    assertEquals(errors.length, 0);
    assertEquals(report.valid, true);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("doctor: empty KG files produce no errors", async () => {
  const dir = await makeTempDir();
  try {
    await setupKg(dir, [], []);
    const report = await inspectKg(dir);
    const errors = report.issues.filter((i) => i.severity === "error");
    assertEquals(errors.length, 0);
    assertEquals(report.valid, true);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("doctor: missing NDJSON files treated as empty (no errors)", async () => {
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(`${dir}/.khive/kg`, { recursive: true });
    // No NDJSON files
    const report = await inspectKg(dir);
    const errors = report.issues.filter((i) => i.severity === "error");
    assertEquals(errors.length, 0);
    assertEquals(report.valid, true);
  } finally {
    await removeDir(dir);
  }
});

// ─── Error cases ──────────────────────────────────────────────────────────────

Deno.test("doctor: invalid JSON in entities.ndjson is INVALID_JSON error", async () => {
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(`${dir}/.khive/kg`, { recursive: true });
    await Deno.writeTextFile(`${dir}/.khive/kg/entities.ndjson`, "not-json\n");
    await Deno.writeTextFile(`${dir}/.khive/kg/edges.ndjson`, "");
    const report = await inspectKg(dir);
    const errors = report.issues.filter((i) => i.code === "INVALID_JSON");
    assertEquals(errors.length >= 1, true);
    assertEquals(errors[0].severity, "error");
    assertEquals(report.valid, false);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("doctor: invalid JSON in edges.ndjson is INVALID_JSON error", async () => {
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(`${dir}/.khive/kg`, { recursive: true });
    await Deno.writeTextFile(`${dir}/.khive/kg/entities.ndjson`, "");
    await Deno.writeTextFile(`${dir}/.khive/kg/edges.ndjson`, "bad-json\n");
    const report = await inspectKg(dir);
    const errors = report.issues.filter((i) => i.code === "INVALID_JSON");
    assertEquals(errors.length >= 1, true);
    assertEquals(report.valid, false);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("doctor: missing entity id is MISSING_FIELD error", async () => {
  const dir = await makeTempDir();
  try {
    await setupKg(dir, [{ kind: "concept", name: "No ID" }], []);
    const report = await inspectKg(dir);
    const errors = report.issues.filter((i) => i.code === "MISSING_FIELD");
    assertEquals(errors.length >= 1, true);
    assertEquals(report.valid, false);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("doctor: missing entity name is MISSING_FIELD error", async () => {
  const dir = await makeTempDir();
  try {
    await setupKg(dir, [{ id: ID_A, kind: "concept" }], []);
    const report = await inspectKg(dir);
    const errors = report.issues.filter((i) => i.code === "MISSING_FIELD");
    assertEquals(errors.length >= 1, true);
    assertEquals(report.valid, false);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("doctor: missing entity kind is MISSING_FIELD error", async () => {
  const dir = await makeTempDir();
  try {
    await setupKg(dir, [{ id: ID_A, name: "No Kind" }], []);
    const report = await inspectKg(dir);
    const errors = report.issues.filter((i) => i.code === "MISSING_FIELD");
    assertEquals(errors.length >= 1, true);
    assertEquals(report.valid, false);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("doctor: duplicate entity id is DUPLICATE_ID error", async () => {
  const dir = await makeTempDir();
  try {
    await setupKg(dir, [ENTITY_A, { ...ENTITY_A, name: "Duplicate" }], []);
    const report = await inspectKg(dir);
    const errors = report.issues.filter((i) => i.code === "DUPLICATE_ID");
    assertEquals(errors.length, 1);
    assertEquals(errors[0].severity, "error");
    assertEquals(report.valid, false);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("doctor: missing edge_id is MISSING_FIELD error", async () => {
  const dir = await makeTempDir();
  try {
    await setupKg(dir, [ENTITY_A, ENTITY_B], [
      { source: ID_B, target: ID_A, relation: "depends_on" },
    ]);
    const report = await inspectKg(dir);
    const errors = report.issues.filter((i) => i.code === "MISSING_FIELD");
    assertEquals(errors.length >= 1, true);
    assertEquals(report.valid, false);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("doctor: duplicate edge_id is DUPLICATE_EDGE_ID error", async () => {
  const dir = await makeTempDir();
  try {
    // Same edge_id, different natural key
    const edge2 = { ...EDGE_AB, edge_id: EDGE_ID_1, relation: "enables" };
    await setupKg(dir, [ENTITY_A, ENTITY_B], [EDGE_AB, edge2]);
    const report = await inspectKg(dir);
    const errors = report.issues.filter((i) => i.code === "DUPLICATE_EDGE_ID");
    assertEquals(errors.length, 1);
    assertEquals(report.valid, false);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("doctor: duplicate natural edge key is DUPLICATE_NATURAL_KEY error", async () => {
  const dir = await makeTempDir();
  try {
    // Different edge_id, same (source, target, relation)
    const edge2 = { ...EDGE_AB, edge_id: EDGE_ID_2 };
    await setupKg(dir, [ENTITY_A, ENTITY_B], [EDGE_AB, edge2]);
    const report = await inspectKg(dir);
    const errors = report.issues.filter((i) => i.code === "DUPLICATE_NATURAL_KEY");
    assertEquals(errors.length, 1);
    assertEquals(report.valid, false);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("doctor: dangling edge reference is DANGLING_REF error", async () => {
  const dir = await makeTempDir();
  try {
    // EDGE_AB references ID_B but only ENTITY_A is present
    await setupKg(dir, [ENTITY_A], [EDGE_AB]);
    const report = await inspectKg(dir);
    const errors = report.issues.filter((i) => i.code === "DANGLING_REF");
    assertEquals(errors.length >= 1, true);
    assertEquals(report.valid, false);
  } finally {
    await removeDir(dir);
  }
});

// ─── Warning cases ────────────────────────────────────────────────────────────

Deno.test("doctor: orphan entity is ORPHAN_ENTITY warning, not error", async () => {
  const dir = await makeTempDir();
  try {
    // ENTITY_A has no edges
    await setupKg(dir, [ENTITY_A], []);
    const report = await inspectKg(dir);
    const warnings = report.issues.filter((i) => i.code === "ORPHAN_ENTITY");
    assertEquals(warnings.length, 1);
    assertEquals(warnings[0].severity, "warning");
    assertEquals(report.valid, true); // warnings don't make it invalid
  } finally {
    await removeDir(dir);
  }
});

Deno.test("doctor: unknown entity kind is UNKNOWN_KIND warning when schema present", async () => {
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(`${dir}/.khive/kg`, { recursive: true });
    await Deno.writeTextFile(
      `${dir}/.khive/kg/schema.yaml`,
      `format_version: "1"\nentity_kinds:\n  - concept\nedge_relations:\n  - relation: contains\n`,
    );
    // ENTITY_B has kind "project" not in schema
    await setupKg(dir, [ENTITY_A, ENTITY_B], [EDGE_AB]);
    const report = await inspectKg(dir);
    const warnings = report.issues.filter((i) => i.code === "UNKNOWN_KIND");
    assertEquals(warnings.length, 1);
    assertEquals(warnings[0].severity, "warning");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("doctor: unknown edge relation is UNKNOWN_RELATION warning when schema present", async () => {
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(`${dir}/.khive/kg`, { recursive: true });
    await Deno.writeTextFile(
      `${dir}/.khive/kg/schema.yaml`,
      `format_version: "1"\nentity_kinds:\n  - concept\n  - project\nedge_relations:\n  - relation: contains\n`,
    );
    // EDGE_AB has relation "depends_on" not in schema
    await setupKg(dir, [ENTITY_A, ENTITY_B], [EDGE_AB]);
    const report = await inspectKg(dir);
    const warnings = report.issues.filter((i) => i.code === "UNKNOWN_RELATION");
    assertEquals(warnings.length, 1);
    assertEquals(warnings[0].severity, "warning");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("doctor: no UNKNOWN_KIND warnings when schema absent", async () => {
  const dir = await makeTempDir();
  try {
    // No schema.yaml — kind checks skipped
    await setupKg(dir, [ENTITY_A, ENTITY_B], [EDGE_AB]);
    const report = await inspectKg(dir);
    const unknownKind = report.issues.filter((i) => i.code === "UNKNOWN_KIND");
    assertEquals(unknownKind.length, 0);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("doctor: warnings alone do not set valid to false", async () => {
  const dir = await makeTempDir();
  try {
    // Only orphans — no actual errors
    await setupKg(dir, [ENTITY_A, ENTITY_B], []);
    const report = await inspectKg(dir);
    const errors = report.issues.filter((i) => i.severity === "error");
    const warnings = report.issues.filter((i) => i.severity === "warning");
    assertEquals(errors.length, 0);
    assertEquals(warnings.length >= 2, true); // at least 2 orphans
    assertEquals(report.valid, true);
  } finally {
    await removeDir(dir);
  }
});
