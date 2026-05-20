/**
 * Tests for the KG validator (ADR-048 field alignment, referential integrity,
 * sort order, composite key deduplication).
 */

import { assertEquals } from "@std/assert";
import { join } from "@std/path";
import { validate } from "./validate.ts";

// ─── Test harness ─────────────────────────────────────────────────────────────

async function makeTempRepo(): Promise<string> {
  const dir = await Deno.makeTempDir({ prefix: "khive_validate_test_" });
  // Minimal schema.yaml so the validator can check kinds/relations.
  await Deno.mkdir(join(dir, ".khive/kg"), { recursive: true });
  await Deno.writeTextFile(
    join(dir, ".khive/kg/schema.yaml"),
    [
      'format_version: "1.0.0"',
      "entity_kinds:",
      "  - concept",
      "  - project",
      "edge_relations:",
      "  - relation: implements",
      "  - relation: depends_on",
    ].join("\n") + "\n",
  );
  return dir;
}

async function removeDir(path: string): Promise<void> {
  await Deno.remove(path, { recursive: true });
}

/** Write entities.ndjson lines into the temp repo. */
async function writeEntities(dir: string, lines: string[]): Promise<void> {
  await Deno.writeTextFile(
    join(dir, ".khive/kg/entities.ndjson"),
    lines.join("\n") + (lines.length > 0 ? "\n" : ""),
  );
}

/** Write edges.ndjson lines into the temp repo. */
async function writeEdges(dir: string, lines: string[]): Promise<void> {
  await Deno.writeTextFile(
    join(dir, ".khive/kg/edges.ndjson"),
    lines.join("\n") + (lines.length > 0 ? "\n" : ""),
  );
}

// ─── Entity tests ─────────────────────────────────────────────────────────────

Deno.test("validate: passes for empty NDJSON files", async () => {
  const dir = await makeTempRepo();
  try {
    await writeEntities(dir, []);
    await writeEdges(dir, []);
    const result = await validate(dir);
    assertEquals(result.valid, true);
    assertEquals(result.errors.length, 0);
    assertEquals(result.entityCount, 0);
    assertEquals(result.edgeCount, 0);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("validate: accepts valid entity line", async () => {
  const dir = await makeTempRepo();
  try {
    await writeEntities(dir, [
      '{"id":"00000000-0000-0000-0000-000000000001","name":"Concept A","kind":"concept"}',
    ]);
    await writeEdges(dir, []);
    const result = await validate(dir);
    assertEquals(result.valid, true);
    assertEquals(result.entityCount, 1);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("validate: rejects entity kind not in schema", async () => {
  const dir = await makeTempRepo();
  try {
    // "dataset" is not in the minimal schema
    await writeEntities(dir, [
      '{"id":"00000000-0000-0000-0000-000000000001","name":"DS","kind":"dataset"}',
    ]);
    await writeEdges(dir, []);
    const result = await validate(dir);
    assertEquals(result.valid, false);
    const kinds = result.errors.some((e) =>
      e.message.includes("not declared in schema.yaml entity_kinds")
    );
    assertEquals(kinds, true);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("validate: rejects duplicate entity id", async () => {
  const dir = await makeTempRepo();
  try {
    const line = '{"id":"00000000-0000-0000-0000-000000000001","name":"A","kind":"concept"}';
    await writeEntities(dir, [line, line]);
    await writeEdges(dir, []);
    const result = await validate(dir);
    assertEquals(result.valid, false);
    assertEquals(result.errors.some((e) => e.message.includes("Duplicate entity id")), true);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("validate: rejects entity sort order violation", async () => {
  const dir = await makeTempRepo();
  try {
    // Second UUID sorts before first — violates UUID-ascending order
    await writeEntities(dir, [
      '{"id":"ffffffff-0000-0000-0000-000000000002","name":"B","kind":"concept"}',
      '{"id":"00000000-0000-0000-0000-000000000001","name":"A","kind":"concept"}',
    ]);
    await writeEdges(dir, []);
    const result = await validate(dir);
    assertEquals(result.valid, false);
    assertEquals(
      result.errors.some((e) => e.message.includes("out of sort order")),
      true,
    );
  } finally {
    await removeDir(dir);
  }
});

// ─── Edge tests (ADR-048 field names) ────────────────────────────────────────

Deno.test("validate: accepts valid edge with ADR-048 field names (edge_id/source/target)", async () => {
  const dir = await makeTempRepo();
  try {
    await writeEntities(dir, [
      '{"id":"00000000-0000-0000-0000-000000000001","name":"A","kind":"concept"}',
      '{"id":"00000000-0000-0000-0000-000000000002","name":"B","kind":"project"}',
    ]);
    await writeEdges(dir, [
      '{"edge_id":"eeeeeeee-0000-0000-0000-000000000001","source":"00000000-0000-0000-0000-000000000002","target":"00000000-0000-0000-0000-000000000001","relation":"implements","weight":1.0,"properties":{}}',
    ]);
    const result = await validate(dir);
    assertEquals(result.valid, true);
    assertEquals(result.edgeCount, 1);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("validate: rejects edge with old id/source_id/target_id field names", async () => {
  const dir = await makeTempRepo();
  try {
    await writeEntities(dir, [
      '{"id":"00000000-0000-0000-0000-000000000001","name":"A","kind":"concept"}',
    ]);
    // Old field names — should fail parse
    await writeEdges(dir, [
      '{"id":"eeeeeeee-0000-0000-0000-000000000001","source_id":"00000000-0000-0000-0000-000000000001","target_id":"00000000-0000-0000-0000-000000000001","relation":"implements"}',
    ]);
    const result = await validate(dir);
    assertEquals(result.valid, false);
    assertEquals(result.errors.some((e) => e.message.includes("Invalid edge")), true);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("validate: rejects dangling edge source (referential integrity)", async () => {
  const dir = await makeTempRepo();
  try {
    await writeEntities(dir, [
      '{"id":"00000000-0000-0000-0000-000000000002","name":"B","kind":"project"}',
    ]);
    // source is a UUID not in entities
    await writeEdges(dir, [
      '{"edge_id":"eeeeeeee-0000-0000-0000-000000000001","source":"00000000-0000-0000-0000-000000000099","target":"00000000-0000-0000-0000-000000000002","relation":"implements","weight":1.0,"properties":{}}',
    ]);
    const result = await validate(dir);
    assertEquals(result.valid, false);
    assertEquals(
      result.errors.some((e) => e.message.includes("does not reference a known entity id")),
      true,
    );
  } finally {
    await removeDir(dir);
  }
});

Deno.test("validate: rejects dangling edge target (referential integrity)", async () => {
  const dir = await makeTempRepo();
  try {
    await writeEntities(dir, [
      '{"id":"00000000-0000-0000-0000-000000000001","name":"A","kind":"concept"}',
    ]);
    // target does not exist
    await writeEdges(dir, [
      '{"edge_id":"eeeeeeee-0000-0000-0000-000000000001","source":"00000000-0000-0000-0000-000000000001","target":"00000000-0000-0000-0000-000000000099","relation":"depends_on","weight":1.0,"properties":{}}',
    ]);
    const result = await validate(dir);
    assertEquals(result.valid, false);
    assertEquals(
      result.errors.some((e) => e.message.includes("does not reference a known entity id")),
      true,
    );
  } finally {
    await removeDir(dir);
  }
});

Deno.test("validate: rejects remote ref target when remote not in schema", async () => {
  const dir = await makeTempRepo();
  try {
    await writeEntities(dir, [
      '{"id":"00000000-0000-0000-0000-000000000001","name":"A","kind":"concept"}',
    ]);
    await writeEdges(dir, [
      '{"edge_id":"eeeeeeee-0000-0000-0000-000000000001","source":"00000000-0000-0000-0000-000000000001","target":"lattice:00000000-0000-0000-0000-000000000099","relation":"depends_on","weight":1.0,"properties":{}}',
    ]);
    const result = await validate(dir);
    const hasRemoteError = result.errors.some((e) => e.message.includes("undeclared remote"));
    assertEquals(hasRemoteError, true);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("validate: rejects duplicate composite key (source, target, relation)", async () => {
  const dir = await makeTempRepo();
  try {
    await writeEntities(dir, [
      '{"id":"00000000-0000-0000-0000-000000000001","name":"A","kind":"concept"}',
      '{"id":"00000000-0000-0000-0000-000000000002","name":"B","kind":"project"}',
    ]);
    const edgeLine =
      '{"edge_id":"eeeeeeee-0000-0000-0000-00000000000%d","source":"00000000-0000-0000-0000-000000000002","target":"00000000-0000-0000-0000-000000000001","relation":"implements","weight":1.0,"properties":{}}';
    await writeEdges(dir, [
      edgeLine.replace("%d", "1"),
      edgeLine.replace("%d", "2"), // different edge_id, same (source, target, relation)
    ]);
    const result = await validate(dir);
    assertEquals(result.valid, false);
    assertEquals(
      result.errors.some((e) => e.message.includes("Duplicate edge composite key")),
      true,
    );
  } finally {
    await removeDir(dir);
  }
});

Deno.test("validate: rejects edge sort order violation", async () => {
  const dir = await makeTempRepo();
  try {
    await writeEntities(dir, [
      '{"id":"00000000-0000-0000-0000-000000000001","name":"A","kind":"concept"}',
      '{"id":"00000000-0000-0000-0000-000000000002","name":"B","kind":"project"}',
    ]);
    // Second edge has a composite key that sorts before the first
    await writeEdges(dir, [
      '{"edge_id":"eeeeeeee-0000-0000-0000-000000000002","source":"00000000-0000-0000-0000-000000000002","target":"00000000-0000-0000-0000-000000000001","relation":"implements","weight":1.0,"properties":{}}',
      '{"edge_id":"eeeeeeee-0000-0000-0000-000000000001","source":"00000000-0000-0000-0000-000000000001","target":"00000000-0000-0000-0000-000000000002","relation":"depends_on","weight":1.0,"properties":{}}',
    ]);
    const result = await validate(dir);
    assertEquals(result.valid, false);
    assertEquals(
      result.errors.some((e) => e.message.includes("out of sort order")),
      true,
    );
  } finally {
    await removeDir(dir);
  }
});

Deno.test("validate: rejects edge relation not in schema", async () => {
  const dir = await makeTempRepo();
  try {
    await writeEntities(dir, [
      '{"id":"00000000-0000-0000-0000-000000000001","name":"A","kind":"concept"}',
      '{"id":"00000000-0000-0000-0000-000000000002","name":"B","kind":"project"}',
    ]);
    // "competes_with" is a valid global relation but not in the minimal schema
    await writeEdges(dir, [
      '{"edge_id":"eeeeeeee-0000-0000-0000-000000000001","source":"00000000-0000-0000-0000-000000000001","target":"00000000-0000-0000-0000-000000000002","relation":"competes_with","weight":1.0,"properties":{}}',
    ]);
    const result = await validate(dir);
    assertEquals(result.valid, false);
    assertEquals(
      result.errors.some((e) => e.message.includes("not declared in schema.yaml edge_relations")),
      true,
    );
  } finally {
    await removeDir(dir);
  }
});
