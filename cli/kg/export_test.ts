/**
 * Tests for `khive kg export`.
 */

import { assertEquals } from "@std/assert";
import { join } from "@std/path";
import { exportArchive, exportCanonical, runExport } from "./export.ts";

// ─── Test harness ─────────────────────────────────────────────────────────────

async function makeTempDir(): Promise<string> {
  return await Deno.makeTempDir({ prefix: "khive_export_test_" });
}

async function removeDir(path: string): Promise<void> {
  await Deno.remove(path, { recursive: true });
}

async function makeKgDir(dir: string): Promise<void> {
  await Deno.mkdir(join(dir, ".khive/kg"), { recursive: true });
}

async function writeEntities(dir: string, lines: string[]): Promise<void> {
  await Deno.writeTextFile(
    join(dir, ".khive/kg/entities.ndjson"),
    lines.join("\n") + (lines.length > 0 ? "\n" : ""),
  );
}

async function writeEdges(dir: string, lines: string[]): Promise<void> {
  await Deno.writeTextFile(
    join(dir, ".khive/kg/edges.ndjson"),
    lines.join("\n") + (lines.length > 0 ? "\n" : ""),
  );
}

/** Capture stdout by temporarily replacing console.log. */
async function captureStdout(fn: () => Promise<void>): Promise<string> {
  const lines: string[] = [];
  const original = console.log;
  const originalErr = console.error;
  console.log = (...args: unknown[]) => {
    lines.push(args.map(String).join(" "));
  };
  // Suppress stderr noise in tests
  console.error = () => {};
  try {
    await fn();
  } finally {
    console.log = original;
    console.error = originalErr;
  }
  return lines.join("\n");
}

// ─── Fixtures ─────────────────────────────────────────────────────────────────

// ADR-048 canonical field order for entities: id, kind, name, ...
const ENTITY_A_LINE =
  '{"id":"00000000-0000-0000-0000-000000000001","kind":"concept","name":"Concept A","description":"First","properties":{},"tags":[]}';

const ENTITY_B_LINE =
  '{"id":"ffffffff-0000-0000-0000-000000000002","kind":"project","name":"Project B","properties":{},"tags":[]}';

const EDGE_LINE =
  '{"edge_id":"eeeeeeee-0000-0000-0000-000000000001","source":"ffffffff-0000-0000-0000-000000000002","target":"00000000-0000-0000-0000-000000000001","relation":"implements","weight":1.0,"properties":{}}';

// ─── exportCanonical (default export) tests ───────────────────────────────────

Deno.test("export: canonical — empty NDJSON files produce empty output", async () => {
  const dir = await makeTempDir();
  try {
    await makeKgDir(dir);
    await writeEntities(dir, []);
    await writeEdges(dir, []);

    await exportCanonical(dir);

    const entitiesText = await Deno.readTextFile(join(dir, ".khive/kg/entities.ndjson"));
    const edgesText = await Deno.readTextFile(join(dir, ".khive/kg/edges.ndjson"));
    assertEquals(entitiesText, "");
    assertEquals(edgesText, "");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("export: canonical — missing NDJSON files treated as empty", async () => {
  const dir = await makeTempDir();
  try {
    // No kg dir at all — readNdjson returns empty for NotFound
    await exportCanonical(dir);
    // .khive/kg/ should be created; files should be empty
    const entitiesText = await Deno.readTextFile(join(dir, ".khive/kg/entities.ndjson"));
    const edgesText = await Deno.readTextFile(join(dir, ".khive/kg/edges.ndjson"));
    assertEquals(entitiesText, "");
    assertEquals(edgesText, "");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("export: canonical — entity written in ADR-048 canonical field order", async () => {
  const dir = await makeTempDir();
  try {
    await makeKgDir(dir);
    // Write entity with non-canonical field order (name before kind)
    const nonCanonical =
      '{"id":"00000000-0000-0000-0000-000000000001","name":"Concept A","kind":"concept","properties":{},"tags":[]}';
    await writeEntities(dir, [nonCanonical]);
    await writeEdges(dir, []);

    await exportCanonical(dir);

    const entitiesText = await Deno.readTextFile(join(dir, ".khive/kg/entities.ndjson"));
    const lines = entitiesText.split("\n").filter((l) => l.trim() !== "");
    assertEquals(lines.length, 1);

    // Canonical order: id, kind, name, ...
    const keys = Object.keys(JSON.parse(lines[0]));
    assertEquals(keys[0], "id");
    assertEquals(keys[1], "kind");
    assertEquals(keys[2], "name");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("export: canonical — idempotent (two exports produce bit-identical files)", async () => {
  const dir = await makeTempDir();
  try {
    await makeKgDir(dir);
    await writeEntities(dir, [ENTITY_A_LINE, ENTITY_B_LINE]);
    await writeEdges(dir, [EDGE_LINE]);

    await exportCanonical(dir);
    const entities1 = await Deno.readTextFile(join(dir, ".khive/kg/entities.ndjson"));
    const edges1 = await Deno.readTextFile(join(dir, ".khive/kg/edges.ndjson"));

    await exportCanonical(dir);
    const entities2 = await Deno.readTextFile(join(dir, ".khive/kg/entities.ndjson"));
    const edges2 = await Deno.readTextFile(join(dir, ".khive/kg/edges.ndjson"));

    assertEquals(entities1, entities2);
    assertEquals(edges1, edges2);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("export: canonical — properties keys sorted alphabetically", async () => {
  const dir = await makeTempDir();
  try {
    await makeKgDir(dir);
    // Properties with keys in reverse alphabetical order
    const withProps =
      '{"id":"00000000-0000-0000-0000-000000000001","kind":"concept","name":"A","properties":{"zzz":"z","aaa":"a","mmm":"m"},"tags":[]}';
    await writeEntities(dir, [withProps]);
    await writeEdges(dir, []);

    await exportCanonical(dir);

    const entitiesText = await Deno.readTextFile(join(dir, ".khive/kg/entities.ndjson"));
    const lines = entitiesText.split("\n").filter((l) => l.trim() !== "");
    const props = JSON.parse(lines[0]).properties as Record<string, unknown>;
    const keys = Object.keys(props);
    assertEquals(keys, ["aaa", "mmm", "zzz"]);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("export: canonical — tags sorted lexicographically", async () => {
  const dir = await makeTempDir();
  try {
    await makeKgDir(dir);
    const withTags =
      '{"id":"00000000-0000-0000-0000-000000000001","kind":"concept","name":"A","properties":{},"tags":["zzz","aaa","mmm"]}';
    await writeEntities(dir, [withTags]);
    await writeEdges(dir, []);

    await exportCanonical(dir);

    const entitiesText = await Deno.readTextFile(join(dir, ".khive/kg/entities.ndjson"));
    const lines = entitiesText.split("\n").filter((l) => l.trim() !== "");
    const tags = JSON.parse(lines[0]).tags as string[];
    assertEquals(tags, ["aaa", "mmm", "zzz"]);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("export: canonical — entities preserved", async () => {
  const dir = await makeTempDir();
  try {
    await makeKgDir(dir);
    await writeEntities(dir, [ENTITY_A_LINE]);
    await writeEdges(dir, []);

    await exportCanonical(dir);

    const entitiesText = await Deno.readTextFile(join(dir, ".khive/kg/entities.ndjson"));
    const lines = entitiesText.split("\n").filter((l) => l.trim() !== "");
    assertEquals(lines.length, 1);
    assertEquals(JSON.parse(lines[0]).id, "00000000-0000-0000-0000-000000000001");
    assertEquals(JSON.parse(lines[0]).name, "Concept A");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("export: canonical — edges preserved", async () => {
  const dir = await makeTempDir();
  try {
    await makeKgDir(dir);
    await writeEntities(dir, [ENTITY_A_LINE, ENTITY_B_LINE]);
    await writeEdges(dir, [EDGE_LINE]);

    await exportCanonical(dir);

    const edgesText = await Deno.readTextFile(join(dir, ".khive/kg/edges.ndjson"));
    const lines = edgesText.split("\n").filter((l) => l.trim() !== "");
    assertEquals(lines.length, 1);
    assertEquals(JSON.parse(lines[0]).edge_id, "eeeeeeee-0000-0000-0000-000000000001");
    assertEquals(JSON.parse(lines[0]).relation, "implements");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("export: canonical — multiple entities preserved in UUID-ascending order", async () => {
  const dir = await makeTempDir();
  try {
    await makeKgDir(dir);
    // A (00000000...) before B (ffffffff...)
    await writeEntities(dir, [ENTITY_A_LINE, ENTITY_B_LINE]);
    await writeEdges(dir, []);

    await exportCanonical(dir);

    const entitiesText = await Deno.readTextFile(join(dir, ".khive/kg/entities.ndjson"));
    const lines = entitiesText.split("\n").filter((l) => l.trim() !== "");
    assertEquals(lines.length, 2);
    assertEquals(JSON.parse(lines[0]).id, "00000000-0000-0000-0000-000000000001");
    assertEquals(JSON.parse(lines[1]).id, "ffffffff-0000-0000-0000-000000000002");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("export: canonical — sorts entities UUID-ascending even when input is reversed", async () => {
  const dir = await makeTempDir();
  try {
    await makeKgDir(dir);
    // Write entities in REVERSE UUID order (B before A).
    await writeEntities(dir, [ENTITY_B_LINE, ENTITY_A_LINE]);
    await writeEdges(dir, []);

    await exportCanonical(dir);

    const entitiesText = await Deno.readTextFile(join(dir, ".khive/kg/entities.ndjson"));
    const lines = entitiesText.split("\n").filter((l) => l.trim() !== "");
    assertEquals(lines.length, 2);
    // After export, A (00000000...) must appear before B (ffffffff...).
    assertEquals(JSON.parse(lines[0]).id, "00000000-0000-0000-0000-000000000001");
    assertEquals(JSON.parse(lines[1]).id, "ffffffff-0000-0000-0000-000000000002");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("export: canonical — sorts edges by composite key (source+target+relation) ascending", async () => {
  const dir = await makeTempDir();
  try {
    await makeKgDir(dir);
    await writeEntities(dir, [ENTITY_A_LINE, ENTITY_B_LINE]);

    // EDGE_LINE has source=ffffffff... (B).
    // EDGE_2_LINE has source=00000000... (A) — should sort first.
    const EDGE_2_LINE =
      '{"edge_id":"eeeeeeee-0000-0000-0000-000000000002","source":"00000000-0000-0000-0000-000000000001","target":"ffffffff-0000-0000-0000-000000000002","relation":"depends_on","weight":0.8,"properties":{}}';

    // Write edges in REVERSE composite-key order (B→A before A→B).
    await writeEdges(dir, [EDGE_LINE, EDGE_2_LINE]);

    await exportCanonical(dir);

    const edgesText = await Deno.readTextFile(join(dir, ".khive/kg/edges.ndjson"));
    const lines = edgesText.split("\n").filter((l) => l.trim() !== "");
    assertEquals(lines.length, 2);
    // After export, edge with source=00000000... must appear first.
    assertEquals(JSON.parse(lines[0]).source, "00000000-0000-0000-0000-000000000001");
    assertEquals(JSON.parse(lines[1]).source, "ffffffff-0000-0000-0000-000000000002");
  } finally {
    await removeDir(dir);
  }
});

// ─── exportArchive (--format archive) tests ────────────────────────────────────

Deno.test("export: archive — empty NDJSON files produce archive with empty arrays", async () => {
  const dir = await makeTempDir();
  try {
    await makeKgDir(dir);
    await writeEntities(dir, []);
    await writeEdges(dir, []);

    const stdout = await captureStdout(() => exportArchive(dir));
    const archive = JSON.parse(stdout);

    assertEquals(archive.format, "khive-kg");
    assertEquals(archive.version, "0.1");
    assertEquals(archive.entities.length, 0);
    assertEquals(archive.edges.length, 0);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("export: archive — missing NDJSON files treated as empty", async () => {
  const dir = await makeTempDir();
  try {
    const stdout = await captureStdout(() => exportArchive(dir));
    const archive = JSON.parse(stdout);
    assertEquals(archive.entities.length, 0);
    assertEquals(archive.edges.length, 0);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("export: archive — entities included in archive output", async () => {
  const dir = await makeTempDir();
  try {
    await makeKgDir(dir);
    await writeEntities(dir, [ENTITY_A_LINE]);
    await writeEdges(dir, []);

    const stdout = await captureStdout(() => exportArchive(dir));
    const archive = JSON.parse(stdout);

    assertEquals(archive.entities.length, 1);
    assertEquals(archive.entities[0].id, "00000000-0000-0000-0000-000000000001");
    assertEquals(archive.entities[0].name, "Concept A");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("export: archive — edges included in archive output", async () => {
  const dir = await makeTempDir();
  try {
    await makeKgDir(dir);
    await writeEntities(dir, [ENTITY_A_LINE, ENTITY_B_LINE]);
    await writeEdges(dir, [EDGE_LINE]);

    const stdout = await captureStdout(() => exportArchive(dir));
    const archive = JSON.parse(stdout);

    assertEquals(archive.edges.length, 1);
    assertEquals(archive.edges[0].edge_id, "eeeeeeee-0000-0000-0000-000000000001");
    assertEquals(archive.edges[0].relation, "implements");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("export: archive — has format, version, namespace, exported_at fields", async () => {
  const dir = await makeTempDir();
  try {
    await makeKgDir(dir);
    await writeEntities(dir, []);
    await writeEdges(dir, []);

    const stdout = await captureStdout(() => exportArchive(dir));
    const archive = JSON.parse(stdout);

    assertEquals(archive.format, "khive-kg");
    assertEquals(archive.version, "0.1");
    assertEquals(archive.namespace, "local");
    assertEquals(typeof archive.exported_at, "string");
    const parsed = Date.parse(archive.exported_at);
    assertEquals(isNaN(parsed), false);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("export: archive — --output writes to file instead of stdout", async () => {
  const dir = await makeTempDir();
  try {
    await makeKgDir(dir);
    await writeEntities(dir, [ENTITY_A_LINE]);
    await writeEdges(dir, []);

    const outFile = join(dir, "output.json");
    await exportArchive(dir, outFile);

    const content = await Deno.readTextFile(outFile);
    const archive = JSON.parse(content);
    assertEquals(archive.format, "khive-kg");
    assertEquals(archive.entities.length, 1);
  } finally {
    await removeDir(dir);
  }
});

// ─── runExport CLI wrapper tests ──────────────────────────────────────────────

Deno.test("export: runExport default writes canonical NDJSON files", async () => {
  const dir = await makeTempDir();
  try {
    await makeKgDir(dir);
    await writeEntities(dir, [ENTITY_A_LINE]);
    await writeEdges(dir, []);

    await runExport(dir, []);

    // Default export writes in-place, not to stdout
    const entitiesText = await Deno.readTextFile(join(dir, ".khive/kg/entities.ndjson"));
    const lines = entitiesText.split("\n").filter((l) => l.trim() !== "");
    assertEquals(lines.length, 1);
    assertEquals(JSON.parse(lines[0]).id, "00000000-0000-0000-0000-000000000001");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("export: runExport --format archive --output flag writes archive file", async () => {
  const dir = await makeTempDir();
  try {
    await makeKgDir(dir);
    await writeEntities(dir, [ENTITY_A_LINE]);
    await writeEdges(dir, []);
    const outFile = join(dir, "via-run-export.json");
    await runExport(dir, ["--format", "archive", "--output", outFile]);
    const content = await Deno.readTextFile(outFile);
    const archive = JSON.parse(content);
    assertEquals(archive.format, "khive-kg");
    assertEquals(archive.entities.length, 1);
  } finally {
    await removeDir(dir);
  }
});
