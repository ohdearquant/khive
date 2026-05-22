/**
 * Tests for `khive kg import`.
 */

import { assertEquals, assertRejects } from "@std/assert";
import { join } from "@std/path";
import { importArchive, recoverImportJournal, runImport } from "./import.ts";

// ─── Test harness ─────────────────────────────────────────────────────────────

async function makeTempDir(): Promise<string> {
  return await Deno.makeTempDir({ prefix: "khive_import_test_" });
}

async function removeDir(path: string): Promise<void> {
  await Deno.remove(path, { recursive: true });
}

/** Write a KgArchive JSON file and return its path. */
async function writeArchive(dir: string, archive: unknown): Promise<string> {
  const path = join(dir, "archive.json");
  await Deno.writeTextFile(path, JSON.stringify(archive));
  return path;
}

/** Read the entities.ndjson content from the repo, split into data lines. */
async function readEntitiesLines(repoRoot: string): Promise<string[]> {
  try {
    const text = await Deno.readTextFile(join(repoRoot, ".khive/kg/entities.ndjson"));
    return text.split("\n").filter((l) => l.trim() !== "");
  } catch {
    return [];
  }
}

/** Read the edges.ndjson content from the repo, split into data lines. */
async function readEdgesLines(repoRoot: string): Promise<string[]> {
  try {
    const text = await Deno.readTextFile(join(repoRoot, ".khive/kg/edges.ndjson"));
    return text.split("\n").filter((l) => l.trim() !== "");
  } catch {
    return [];
  }
}

/** Check whether a file exists. */
async function fileExists(path: string): Promise<boolean> {
  try {
    await Deno.stat(path);
    return true;
  } catch {
    return false;
  }
}

// ─── Fixtures ─────────────────────────────────────────────────────────────────

const ENTITY_A = {
  id: "00000000-0000-0000-0000-000000000001",
  kind: "concept",
  name: "Concept A",
  description: "First concept",
  properties: {},
  tags: [],
};

const ENTITY_B = {
  id: "ffffffff-0000-0000-0000-000000000002",
  kind: "project",
  name: "Project B",
  properties: {},
  tags: [],
};

const EDGE_1 = {
  edge_id: "eeeeeeee-0000-0000-0000-000000000001",
  source: "ffffffff-0000-0000-0000-000000000002",
  target: "00000000-0000-0000-0000-000000000001",
  relation: "implements",
  weight: 1.0,
  properties: {},
};

// EDGE_SELF references only ENTITY_A (source == target), used in tests that
// only have a single entity in the archive.
const EDGE_SELF = {
  edge_id: "eeeeeeee-0000-0000-0000-000000000099",
  source: "00000000-0000-0000-0000-000000000001",
  target: "00000000-0000-0000-0000-000000000001",
  relation: "annotates",
  weight: 1.0,
  properties: {},
};

// ─── Basic success tests ──────────────────────────────────────────────────────

Deno.test("import: creates .khive/kg/ directory if absent", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [],
      edges: [],
    });
    await importArchive(dir, archivePath);
    const stat = await Deno.stat(join(dir, ".khive/kg"));
    assertEquals(stat.isDirectory, true);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: empty archive writes empty NDJSON files", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [],
      edges: [],
    });
    await importArchive(dir, archivePath);
    const entityLines = await readEntitiesLines(dir);
    const edgeLines = await readEdgesLines(dir);
    assertEquals(entityLines.length, 0);
    assertEquals(edgeLines.length, 0);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: writes entities sorted UUID-ascending", async () => {
  const dir = await makeTempDir();
  try {
    // Provide entities in reverse UUID order — import must sort them
    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [ENTITY_B, ENTITY_A], // B sorts after A by UUID
      edges: [],
    });
    await importArchive(dir, archivePath);
    const lines = await readEntitiesLines(dir);
    assertEquals(lines.length, 2);
    const first = JSON.parse(lines[0]);
    const second = JSON.parse(lines[1]);
    // A (00000000...) should appear before B (ffffffff...)
    assertEquals(first.id, ENTITY_A.id);
    assertEquals(second.id, ENTITY_B.id);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: writes edges sorted by composite key (source+target+relation)", async () => {
  const dir = await makeTempDir();
  try {
    const edge2 = {
      edge_id: "eeeeeeee-0000-0000-0000-000000000002",
      source: "00000000-0000-0000-0000-000000000001",
      target: "ffffffff-0000-0000-0000-000000000002",
      relation: "depends_on",
      weight: 0.8,
      properties: {},
    };
    // edge2 (source=00000000...) should sort before edge1 (source=ffffffff...)
    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [ENTITY_A, ENTITY_B],
      edges: [EDGE_1, edge2],
    });
    await importArchive(dir, archivePath);
    const lines = await readEdgesLines(dir);
    assertEquals(lines.length, 2);
    const first = JSON.parse(lines[0]);
    const second = JSON.parse(lines[1]);
    assertEquals(first.edge_id, edge2.edge_id);
    assertEquals(second.edge_id, EDGE_1.edge_id);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: preserves all entity fields", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [ENTITY_A],
      edges: [], // no edges — single entity, no referential integrity issue
    });
    await importArchive(dir, archivePath);
    const lines = await readEntitiesLines(dir);
    assertEquals(lines.length, 1);
    const stored = JSON.parse(lines[0]);
    assertEquals(stored.id, ENTITY_A.id);
    assertEquals(stored.name, ENTITY_A.name);
    assertEquals(stored.kind, ENTITY_A.kind);
    assertEquals(stored.description, ENTITY_A.description);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: preserves all edge fields", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [ENTITY_A, ENTITY_B],
      edges: [EDGE_1],
    });
    await importArchive(dir, archivePath);
    const lines = await readEdgesLines(dir);
    assertEquals(lines.length, 1);
    const stored = JSON.parse(lines[0]);
    assertEquals(stored.edge_id, EDGE_1.edge_id);
    assertEquals(stored.source, EDGE_1.source);
    assertEquals(stored.target, EDGE_1.target);
    assertEquals(stored.relation, EDGE_1.relation);
    assertEquals(stored.weight, EDGE_1.weight);
  } finally {
    await removeDir(dir);
  }
});

// ─── Format / version rejection tests ────────────────────────────────────────

Deno.test("import: rejects archive with wrong format", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath = await writeArchive(dir, {
      format: "other-format",
      version: "0.1",
      entities: [],
      edges: [],
    });
    await assertRejects(
      () => importArchive(dir, archivePath),
      Error,
      "khive-kg",
    );
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: rejects archive with wrong version", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.2",
      entities: [],
      edges: [],
    });
    await assertRejects(
      () => importArchive(dir, archivePath),
      Error,
      "0.1",
    );
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: rejects archive with entity missing id", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [{ kind: "concept", name: "No ID" }],
      edges: [],
    });
    await assertRejects(
      () => importArchive(dir, archivePath),
      Error,
    );
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: rejects archive with edge missing edge_id", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [ENTITY_A],
      edges: [{
        source: ENTITY_A.id,
        target: ENTITY_A.id,
        relation: "implements",
      }],
    });
    await assertRejects(
      () => importArchive(dir, archivePath),
      Error,
      "edge_id",
    );
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: rejects non-existent archive file", async () => {
  const dir = await makeTempDir();
  try {
    await assertRejects(
      () => importArchive(dir, join(dir, "does_not_exist.json")),
      Error,
      "not found",
    );
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: rejects invalid JSON archive", async () => {
  const dir = await makeTempDir();
  try {
    const badPath = join(dir, "bad.json");
    await Deno.writeTextFile(badPath, "not json at all {{");
    await assertRejects(
      () => importArchive(dir, badPath),
      Error,
    );
  } finally {
    await removeDir(dir);
  }
});

// ─── Closed taxonomy validation tests ─────────────────────────────────────────

Deno.test("import: rejects invalid entity kind", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [{
        id: "00000000-0000-0000-0000-000000000001",
        kind: "paper", // not a valid kind
        name: "Bad Kind",
        properties: {},
        tags: [],
      }],
      edges: [],
    });
    await assertRejects(
      () => importArchive(dir, archivePath),
      Error,
    );
    // No mutation — .khive/kg/ should not have been created with data
    const lines = await readEntitiesLines(dir);
    assertEquals(lines.length, 0);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: rejects invalid edge relation", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [ENTITY_A, ENTITY_B],
      edges: [{
        edge_id: "eeeeeeee-0000-0000-0000-000000000001",
        source: ENTITY_B.id,
        target: ENTITY_A.id,
        relation: "related_to", // not a valid relation
        weight: 1.0,
        properties: {},
      }],
    });
    await assertRejects(
      () => importArchive(dir, archivePath),
      Error,
    );
    // No mutation
    const edgeLines = await readEdgesLines(dir);
    assertEquals(edgeLines.length, 0);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: rejects dangling edge source (not in entity list)", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [ENTITY_A], // ENTITY_B is NOT included
      edges: [{
        edge_id: "eeeeeeee-0000-0000-0000-000000000001",
        source: ENTITY_B.id, // dangling — B not in entities
        target: ENTITY_A.id,
        relation: "implements",
        weight: 1.0,
        properties: {},
      }],
    });
    await assertRejects(
      () => importArchive(dir, archivePath),
      Error,
    );
    // No mutation
    const edgeLines = await readEdgesLines(dir);
    assertEquals(edgeLines.length, 0);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: rejects dangling edge target (not in entity list)", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [ENTITY_A], // ENTITY_B is NOT included
      edges: [{
        edge_id: "eeeeeeee-0000-0000-0000-000000000001",
        source: ENTITY_A.id,
        target: ENTITY_B.id, // dangling — B not in entities
        relation: "implements",
        weight: 1.0,
        properties: {},
      }],
    });
    await assertRejects(
      () => importArchive(dir, archivePath),
      Error,
    );
    const edgeLines = await readEdgesLines(dir);
    assertEquals(edgeLines.length, 0);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: rejects duplicate entity UUIDs", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [
        ENTITY_A,
        { ...ENTITY_A, name: "Duplicate A" }, // same UUID as ENTITY_A
      ],
      edges: [],
    });
    await assertRejects(
      () => importArchive(dir, archivePath),
      Error,
    );
    const entityLines = await readEntitiesLines(dir);
    assertEquals(entityLines.length, 0);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: rejects duplicate edge composite keys (source+target+relation)", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [ENTITY_A, ENTITY_B],
      edges: [
        EDGE_1,
        { ...EDGE_1, edge_id: "eeeeeeee-0000-0000-0000-000000000099" }, // same source+target+relation
      ],
    });
    await assertRejects(
      () => importArchive(dir, archivePath),
      Error,
    );
    const edgeLines = await readEdgesLines(dir);
    assertEquals(edgeLines.length, 0);
  } finally {
    await removeDir(dir);
  }
});

// ─── Overwrite policy tests ───────────────────────────────────────────────────

Deno.test("import: refuses to overwrite existing entities.ndjson without --overwrite", async () => {
  const dir = await makeTempDir();
  try {
    // Create existing NDJSON files
    await Deno.mkdir(join(dir, ".khive/kg"), { recursive: true });
    await Deno.writeTextFile(join(dir, ".khive/kg/entities.ndjson"), "existing\n");
    await Deno.writeTextFile(join(dir, ".khive/kg/edges.ndjson"), "");

    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [ENTITY_A],
      edges: [],
    });

    await assertRejects(
      () => importArchive(dir, archivePath),
      Error,
      "already exists",
    );

    // Original content must be intact
    const content = await Deno.readTextFile(join(dir, ".khive/kg/entities.ndjson"));
    assertEquals(content, "existing\n");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: --overwrite replaces existing NDJSON files", async () => {
  const dir = await makeTempDir();
  try {
    // Create existing NDJSON files
    await Deno.mkdir(join(dir, ".khive/kg"), { recursive: true });
    await Deno.writeTextFile(join(dir, ".khive/kg/entities.ndjson"), "old content\n");
    await Deno.writeTextFile(join(dir, ".khive/kg/edges.ndjson"), "");

    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [ENTITY_A],
      edges: [],
    });

    // Should succeed with overwrite
    await importArchive(dir, archivePath, { overwrite: true });

    const lines = await readEntitiesLines(dir);
    assertEquals(lines.length, 1);
    assertEquals(JSON.parse(lines[0]).id, ENTITY_A.id);
  } finally {
    await removeDir(dir);
  }
});

// ─── Canonical field order tests ──────────────────────────────────────────────

Deno.test("import: writes entities in canonical ADR-048 field order", async () => {
  const dir = await makeTempDir();
  try {
    // Input has non-canonical field order (name before kind)
    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [{
        id: "00000000-0000-0000-0000-000000000001",
        name: "Concept A",
        kind: "concept",
        properties: {},
        tags: [],
      }],
      edges: [],
    });
    await importArchive(dir, archivePath);
    const lines = await readEntitiesLines(dir);
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

Deno.test("import: writes edges in canonical ADR-048 field order", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [ENTITY_A, ENTITY_B],
      edges: [EDGE_1],
    });
    await importArchive(dir, archivePath);
    const lines = await readEdgesLines(dir);
    assertEquals(lines.length, 1);
    // Canonical order: edge_id, source, target, relation, weight, properties, ...
    const keys = Object.keys(JSON.parse(lines[0]));
    assertEquals(keys[0], "edge_id");
    assertEquals(keys[1], "source");
    assertEquals(keys[2], "target");
    assertEquals(keys[3], "relation");
  } finally {
    await removeDir(dir);
  }
});

// ─── Other tests ──────────────────────────────────────────────────────────────

Deno.test("import: dispatches to importArchive with positional archive argument", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath = join(dir, "smoke.json");
    await Deno.writeTextFile(
      archivePath,
      JSON.stringify({ format: "khive-kg", version: "0.1", entities: [], edges: [] }),
    );
    await runImport(dir, [archivePath]);
    const entityLines = await readEntitiesLines(dir);
    assertEquals(entityLines.length, 0);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: sort order is stable (single entity, self-referencing edge)", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [ENTITY_A],
      edges: [EDGE_SELF], // source == target == ENTITY_A.id
    });
    await importArchive(dir, archivePath);
    const entityLines = await readEntitiesLines(dir);
    const edgeLines = await readEdgesLines(dir);
    assertEquals(entityLines.length, 1);
    assertEquals(edgeLines.length, 1);
    assertEquals(JSON.parse(entityLines[0]).id, ENTITY_A.id);
    assertEquals(JSON.parse(edgeLines[0]).edge_id, EDGE_SELF.edge_id);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: idempotent — re-import with --overwrite produces same NDJSON", async () => {
  const dir = await makeTempDir();
  try {
    const archive = {
      format: "khive-kg",
      version: "0.1",
      entities: [ENTITY_A, ENTITY_B],
      edges: [EDGE_1],
    };
    const archivePath = await writeArchive(dir, archive);
    await importArchive(dir, archivePath);
    const entityLines1 = await readEntitiesLines(dir);
    const edgeLines1 = await readEdgesLines(dir);

    await importArchive(dir, archivePath, { overwrite: true });
    const entityLines2 = await readEntitiesLines(dir);
    const edgeLines2 = await readEdgesLines(dir);

    assertEquals(entityLines1, entityLines2);
    assertEquals(edgeLines1, edgeLines2);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: --overwrite preserves originals on mid-publish failure", async () => {
  // Arrange: a valid repo with known original content in both NDJSON files.
  const dir = await makeTempDir();
  try {
    await Deno.mkdir(join(dir, ".khive/kg"), { recursive: true });

    const originalEntities =
      '{"id":"00000000-0000-0000-0000-000000000001","kind":"concept","name":"Original A","properties":{},"tags":[]}\n';
    const originalEdges =
      '{"edge_id":"eeeeeeee-0000-0000-0000-000000000099","source":"00000000-0000-0000-0000-000000000001","target":"00000000-0000-0000-0000-000000000001","relation":"annotates","weight":1.0,"properties":{}}\n';
    await Deno.writeTextFile(join(dir, ".khive/kg/entities.ndjson"), originalEntities);
    await Deno.writeTextFile(join(dir, ".khive/kg/edges.ndjson"), originalEdges);

    // Record original bytes for post-failure comparison.
    const originalEntitiesBytes = await Deno.readFile(join(dir, ".khive/kg/entities.ndjson"));
    const originalEdgesBytes = await Deno.readFile(join(dir, ".khive/kg/edges.ndjson"));

    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [ENTITY_A, ENTITY_B],
      edges: [EDGE_1],
    });

    // Act: inject a fault between the two publish renames using _afterFirstRename.
    // After the first rename (entities staged → dest) succeeds, the hook throws,
    // simulating a mid-publish IO failure before the second rename (edges) runs.
    let threw = false;
    try {
      await importArchive(dir, archivePath, {
        overwrite: true,
        _afterFirstRename: () => {
          throw new Error("injected mid-publish failure");
        },
      });
    } catch {
      threw = true;
    }
    assertEquals(threw, true, "importArchive should have thrown on mid-publish failure");

    // Assert: original entities.ndjson is byte-identical to what we started with.
    const afterEntitiesBytes = await Deno.readFile(join(dir, ".khive/kg/entities.ndjson"));
    assertEquals(
      afterEntitiesBytes,
      originalEntitiesBytes,
      "entities.ndjson must be byte-identical to original after mid-publish failure",
    );

    // Assert: original edges.ndjson is byte-identical to what we started with.
    const afterEdgesBytes = await Deno.readFile(join(dir, ".khive/kg/edges.ndjson"));
    assertEquals(
      afterEdgesBytes,
      originalEdgesBytes,
      "edges.ndjson must be byte-identical to original after mid-publish failure",
    );

    // Assert: no stray .bak files left behind.
    assertEquals(
      await fileExists(join(dir, ".khive/kg/entities.ndjson.bak")),
      false,
      "entities.ndjson.bak must be cleaned up",
    );
    assertEquals(
      await fileExists(join(dir, ".khive/kg/edges.ndjson.bak")),
      false,
      "edges.ndjson.bak must be cleaned up",
    );
  } finally {
    await removeDir(dir);
  }
});

Deno.test("import: no entities.ndjson created on validation failure", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath = await writeArchive(dir, {
      format: "khive-kg",
      version: "0.1",
      entities: [{
        id: "00000000-0000-0000-0000-000000000001",
        kind: "invalid_kind",
        name: "Bad",
        properties: {},
        tags: [],
      }],
      edges: [],
    });

    try {
      await importArchive(dir, archivePath);
    } catch {
      // expected
    }

    const exists = await fileExists(join(dir, ".khive/kg/entities.ndjson"));
    assertEquals(exists, false);
  } finally {
    await removeDir(dir);
  }
});

// ─── Subprocess crash regression tests ───────────────────────────────────────
//
// These tests spawn a real child process that runs importArchive and kills it
// mid-publish using KHIVE_TEST_CRASH_AFTER.  They prove that the journal-based
// recovery protocol (not just caught-error rollback) handles process death.

/**
 * Path to import.ts so the subprocess script can import it.
 * import.meta.url → file:///.../.../cli/kg/import_test.ts
 * ../../ → file:///.../.../cli/
 * + kg/import.ts
 */
const IMPORT_TS_URL = new URL("./import.ts", import.meta.url).href;

/**
 * Write a subprocess harness script that calls importArchive using paths
 * from environment variables, then exits.  The KHIVE_TEST_CRASH_AFTER env
 * var causes it to crash at the configured point with exit code 42.
 */
async function writeHarnessScript(dir: string): Promise<string> {
  const scriptPath = join(dir, "_crash_harness.ts");
  await Deno.writeTextFile(
    scriptPath,
    `
import { importArchive } from "${IMPORT_TS_URL}";
const repoRoot = Deno.env.get("KHIVE_TEST_REPO_ROOT")!;
const archivePath = Deno.env.get("KHIVE_TEST_ARCHIVE_PATH")!;
await importArchive(repoRoot, archivePath, { overwrite: true });
`,
  );
  return scriptPath;
}

/**
 * Spawn the harness script with the given crash point and return the exit code.
 */
async function runCrashHarness(
  harnessScript: string,
  repoRoot: string,
  archivePath: string,
  crashAfter: string,
): Promise<number> {
  const configPath = new URL("../deno.json", import.meta.url).pathname;
  const cmd = new Deno.Command(Deno.execPath(), {
    args: ["run", "--allow-all", "--config", configPath, harnessScript],
    env: {
      KHIVE_TEST_REPO_ROOT: repoRoot,
      KHIVE_TEST_ARCHIVE_PATH: archivePath,
      KHIVE_TEST_CRASH_AFTER: crashAfter,
      KHIVE_DEV: "1",
      // Propagate HOME so Deno can resolve stdlib cache.
      HOME: Deno.env.get("HOME") ?? "",
      DENO_DIR: Deno.env.get("DENO_DIR") ?? "",
    },
    stdout: "piped",
    stderr: "piped",
  });
  const result = await cmd.output();
  return result.code;
}

// ── Crash after journal written (before any renames) ──────────────────────────

Deno.test(
  "crash-recovery: crash after journal written rolls back with no partial live state",
  async () => {
    const dir = await makeTempDir();
    try {
      // Arrange: repo with known original NDJSON files.
      await Deno.mkdir(join(dir, ".khive/kg"), { recursive: true });
      const originalEntities =
        '{"id":"00000000-0000-0000-0000-000000000001","kind":"concept","name":"Original A","properties":{},"tags":[]}\n';
      const originalEdges =
        '{"edge_id":"eeeeeeee-0000-0000-0000-000000000099","source":"00000000-0000-0000-0000-000000000001","target":"00000000-0000-0000-0000-000000000001","relation":"annotates","weight":1,"properties":{}}\n';
      await Deno.writeTextFile(join(dir, ".khive/kg/entities.ndjson"), originalEntities);
      await Deno.writeTextFile(join(dir, ".khive/kg/edges.ndjson"), originalEdges);

      const originalEntitiesBytes = await Deno.readFile(
        join(dir, ".khive/kg/entities.ndjson"),
      );
      const originalEdgesBytes = await Deno.readFile(join(dir, ".khive/kg/edges.ndjson"));

      const archivePath = await writeArchive(dir, {
        format: "khive-kg",
        version: "0.1",
        entities: [ENTITY_A, ENTITY_B],
        edges: [EDGE_1],
      });
      const harnessScript = await writeHarnessScript(dir);

      // Act: spawn subprocess, crash after journal written but before any renames.
      const exitCode = await runCrashHarness(
        harnessScript,
        dir,
        archivePath,
        "journal_written",
      );
      assertEquals(exitCode, 42, "subprocess should exit 42 at crash point");

      // Verify the journal exists (status=pending).
      const journalExists = await fileExists(join(dir, ".khive/.import-journal.json"));
      assertEquals(journalExists, true, "journal must exist after crash");
      const journalText = await Deno.readTextFile(join(dir, ".khive/.import-journal.json"));
      const journal = JSON.parse(journalText);
      assertEquals(journal.status, "pending", "journal status must be pending");

      // Act: recover.
      const recovered = await recoverImportJournal(dir);
      assertEquals(recovered, "rolled_back");

      // Assert: live files are byte-identical to originals.
      const afterEntitiesBytes = await Deno.readFile(join(dir, ".khive/kg/entities.ndjson"));
      assertEquals(
        afterEntitiesBytes,
        originalEntitiesBytes,
        "entities.ndjson must be byte-identical to original after crash+recovery",
      );
      const afterEdgesBytes = await Deno.readFile(join(dir, ".khive/kg/edges.ndjson"));
      assertEquals(
        afterEdgesBytes,
        originalEdgesBytes,
        "edges.ndjson must be byte-identical to original after crash+recovery",
      );

      // Assert: no stray .bak files.
      assertEquals(
        await fileExists(join(dir, ".khive/kg/entities.ndjson.bak")),
        false,
        "entities.ndjson.bak must be cleaned up",
      );
      assertEquals(
        await fileExists(join(dir, ".khive/kg/edges.ndjson.bak")),
        false,
        "edges.ndjson.bak must be cleaned up",
      );

      // Assert: no stray staging dirs or journal.
      assertEquals(
        await fileExists(join(dir, ".khive/.import-journal.json")),
        false,
        "journal must be removed after recovery",
      );
    } finally {
      await removeDir(dir);
    }
  },
);

// ── Crash after first rename (entities staged→live, before edges) ─────────────

Deno.test(
  "crash-recovery: crash after first rename rolls back with no partial live state",
  async () => {
    const dir = await makeTempDir();
    try {
      // Arrange: repo with known original NDJSON files.
      await Deno.mkdir(join(dir, ".khive/kg"), { recursive: true });
      const originalEntities =
        '{"id":"00000000-0000-0000-0000-000000000001","kind":"concept","name":"Original A","properties":{},"tags":[]}\n';
      const originalEdges =
        '{"edge_id":"eeeeeeee-0000-0000-0000-000000000099","source":"00000000-0000-0000-0000-000000000001","target":"00000000-0000-0000-0000-000000000001","relation":"annotates","weight":1,"properties":{}}\n';
      await Deno.writeTextFile(join(dir, ".khive/kg/entities.ndjson"), originalEntities);
      await Deno.writeTextFile(join(dir, ".khive/kg/edges.ndjson"), originalEdges);

      const originalEntitiesBytes = await Deno.readFile(
        join(dir, ".khive/kg/entities.ndjson"),
      );
      const originalEdgesBytes = await Deno.readFile(join(dir, ".khive/kg/edges.ndjson"));

      const archivePath = await writeArchive(dir, {
        format: "khive-kg",
        version: "0.1",
        entities: [ENTITY_A, ENTITY_B],
        edges: [EDGE_1],
      });
      const harnessScript = await writeHarnessScript(dir);

      // Act: spawn subprocess, crash after entities renamed but before edges.
      // This is the exact scenario codex observed: new entities.ndjson present,
      // missing live edges.ndjson, .bak files left, staging dir left.
      const exitCode = await runCrashHarness(
        harnessScript,
        dir,
        archivePath,
        "first_rename",
      );
      assertEquals(exitCode, 42, "subprocess should exit 42 at crash point");

      // Verify partial state: entities.ndjson has NEW content (renamed into place).
      // This is the partial state codex found — prove we can recover from it.
      const partialEntities = await Deno.readFile(join(dir, ".khive/kg/entities.ndjson"));
      assertEquals(
        partialEntities.length !== originalEntitiesBytes.length ||
          !partialEntities.every((b, i) => b === originalEntitiesBytes[i]),
        true,
        "entities.ndjson should have new content after first rename (proving the crash is real)",
      );

      // Verify the journal exists (status=pending).
      const journalExists = await fileExists(join(dir, ".khive/.import-journal.json"));
      assertEquals(journalExists, true, "journal must exist after crash");
      const journalText = await Deno.readTextFile(join(dir, ".khive/.import-journal.json"));
      const journal = JSON.parse(journalText);
      assertEquals(journal.status, "pending", "journal status must be pending");

      // Act: recover.
      const recovered = await recoverImportJournal(dir);
      assertEquals(recovered, "rolled_back");

      // Assert: live files are byte-identical to originals.
      const afterEntitiesBytes = await Deno.readFile(join(dir, ".khive/kg/entities.ndjson"));
      assertEquals(
        afterEntitiesBytes,
        originalEntitiesBytes,
        "entities.ndjson must be byte-identical to original after crash+recovery",
      );
      const afterEdgesBytes = await Deno.readFile(join(dir, ".khive/kg/edges.ndjson"));
      assertEquals(
        afterEdgesBytes,
        originalEdgesBytes,
        "edges.ndjson must be byte-identical to original after crash+recovery",
      );

      // Assert: no stray .bak files.
      assertEquals(
        await fileExists(join(dir, ".khive/kg/entities.ndjson.bak")),
        false,
        "entities.ndjson.bak must be cleaned up",
      );
      assertEquals(
        await fileExists(join(dir, ".khive/kg/edges.ndjson.bak")),
        false,
        "edges.ndjson.bak must be cleaned up",
      );

      // Assert: no journal and no staging dir.
      assertEquals(
        await fileExists(join(dir, ".khive/.import-journal.json")),
        false,
        "journal must be removed after recovery",
      );
    } finally {
      await removeDir(dir);
    }
  },
);

// ── Roll-forward: crash between second rename and journal deletion ─────────────

Deno.test(
  "crash-recovery: status=committed journal rolls forward and cleans up .bak files",
  async () => {
    // Simulate the state that would exist if the process crashed after both
    // renames completed and markJournalCommitted ran, but before .bak cleanup.
    // We construct this state directly (no subprocess needed).
    const dir = await makeTempDir();
    try {
      await Deno.mkdir(join(dir, ".khive/kg"), { recursive: true });
      await Deno.mkdir(join(dir, ".khive"), { recursive: true });

      // Write the new live files (as if renames completed).
      const newEntities =
        '{"id":"00000000-0000-0000-0000-000000000001","kind":"concept","name":"New A","properties":{},"tags":[]}\n';
      const newEdges =
        '{"edge_id":"eeeeeeee-0000-0000-0000-000000000099","source":"00000000-0000-0000-0000-000000000001","target":"00000000-0000-0000-0000-000000000001","relation":"annotates","weight":1,"properties":{}}\n';
      await Deno.writeTextFile(join(dir, ".khive/kg/entities.ndjson"), newEntities);
      await Deno.writeTextFile(join(dir, ".khive/kg/edges.ndjson"), newEdges);

      // Write orphaned .bak files (originals not yet cleaned up).
      const oldEntities =
        '{"id":"00000000-0000-0000-0000-000000000001","kind":"concept","name":"Old A","properties":{},"tags":[]}\n';
      const oldEdges = "";
      await Deno.writeTextFile(
        join(dir, ".khive/kg/entities.ndjson.bak"),
        oldEntities,
      );
      await Deno.writeTextFile(join(dir, ".khive/kg/edges.ndjson.bak"), oldEdges);

      // Write a committed journal.
      const journal = {
        staging_dir: join(dir, ".khive-import-tmp-gone"),
        target_dir: join(dir, ".khive/kg"),
        files_to_swap: [
          {
            staged: join(dir, ".khive-import-tmp-gone/.khive/kg/entities.ndjson"),
            live: join(dir, ".khive/kg/entities.ndjson"),
            bak: join(dir, ".khive/kg/entities.ndjson.bak"),
          },
          {
            staged: join(dir, ".khive-import-tmp-gone/.khive/kg/edges.ndjson"),
            live: join(dir, ".khive/kg/edges.ndjson"),
            bak: join(dir, ".khive/kg/edges.ndjson.bak"),
          },
        ],
        status: "committed",
        timestamp: new Date().toISOString(),
      };
      await Deno.writeTextFile(
        join(dir, ".khive/.import-journal.json"),
        JSON.stringify(journal, null, 2),
      );

      // Act: recover.
      const result = await recoverImportJournal(dir);
      assertEquals(result, "rolled_forward");

      // Assert: live files are untouched (new content).
      const entitiesContent = await Deno.readTextFile(
        join(dir, ".khive/kg/entities.ndjson"),
      );
      assertEquals(entitiesContent, newEntities, "live entities.ndjson must be preserved");
      const edgesContent = await Deno.readTextFile(join(dir, ".khive/kg/edges.ndjson"));
      assertEquals(edgesContent, newEdges, "live edges.ndjson must be preserved");

      // Assert: .bak files deleted.
      assertEquals(
        await fileExists(join(dir, ".khive/kg/entities.ndjson.bak")),
        false,
        "entities.ndjson.bak must be deleted by roll-forward",
      );
      assertEquals(
        await fileExists(join(dir, ".khive/kg/edges.ndjson.bak")),
        false,
        "edges.ndjson.bak must be deleted by roll-forward",
      );

      // Assert: journal deleted.
      assertEquals(
        await fileExists(join(dir, ".khive/.import-journal.json")),
        false,
        "journal must be deleted by roll-forward",
      );
    } finally {
      await removeDir(dir);
    }
  },
);

// ── Recovery is idempotent ─────────────────────────────────────────────────────

Deno.test("crash-recovery: recoverImportJournal is idempotent (no journal = null)", async () => {
  const dir = await makeTempDir();
  try {
    // No journal present — should return null without error.
    const result1 = await recoverImportJournal(dir);
    assertEquals(result1, null);

    // Calling again is also safe.
    const result2 = await recoverImportJournal(dir);
    assertEquals(result2, null);
  } finally {
    await removeDir(dir);
  }
});

// ─── --on-conflict tests ──────────────────────────────────────────────────────

/** Write an archive with specific entities and edges. */
async function writeConflictArchive(
  dir: string,
  entities: unknown[],
  edges: unknown[],
  filename = "archive.json",
): Promise<string> {
  const path = join(dir, filename);
  await Deno.writeTextFile(
    path,
    JSON.stringify({ format: "khive-kg", version: "0.1", entities, edges }),
  );
  return path;
}

const CONFLICT_ENTITY_A = {
  id: "aaaaaaaa-bbbb-cccc-dddd-000000000001",
  kind: "concept",
  name: "Original A",
  properties: { x: 1 },
  tags: ["alpha"],
};

const CONFLICT_ENTITY_A_UPDATED = {
  id: "aaaaaaaa-bbbb-cccc-dddd-000000000001", // same id
  kind: "concept",
  name: "Updated A",
  properties: { y: 2 },
  tags: ["beta"],
};

const CONFLICT_ENTITY_NEW = {
  id: "aaaaaaaa-bbbb-cccc-dddd-000000000002",
  kind: "project",
  name: "New Entity",
  properties: {},
  tags: [],
};

Deno.test("on-conflict: no existing files acts like normal import", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath = await writeConflictArchive(dir, [CONFLICT_ENTITY_A], []);
    // With --on-conflict skip and no existing files, should succeed
    await importArchive(dir, archivePath, { onConflict: "skip" });
    const lines = await readEntitiesLines(dir);
    assertEquals(lines.length, 1);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("on-conflict skip: existing record is kept, new records are added", async () => {
  const dir = await makeTempDir();
  try {
    // First import: establish base
    const archivePath1 = await writeConflictArchive(dir, [CONFLICT_ENTITY_A], []);
    await importArchive(dir, archivePath1);

    // Second import: conflicting entity + new entity, policy = skip
    const archivePath2 = await writeConflictArchive(
      dir,
      [CONFLICT_ENTITY_A_UPDATED, CONFLICT_ENTITY_NEW],
      [],
    );
    await importArchive(dir, archivePath2, { onConflict: "skip" });

    const lines = await readEntitiesLines(dir);
    assertEquals(lines.length, 2); // original A + new entity

    // Original A should be preserved (name not updated)
    const entities = lines.map((l) => JSON.parse(l) as Record<string, unknown>);
    const entityA = entities.find((e) => e["id"] === CONFLICT_ENTITY_A.id);
    assertEquals(entityA?.["name"], "Original A");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("on-conflict replace: incoming record replaces existing", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath1 = await writeConflictArchive(dir, [CONFLICT_ENTITY_A], []);
    await importArchive(dir, archivePath1);

    const archivePath2 = await writeConflictArchive(
      dir,
      [CONFLICT_ENTITY_A_UPDATED, CONFLICT_ENTITY_NEW],
      [],
    );
    await importArchive(dir, archivePath2, { onConflict: "replace" });

    const lines = await readEntitiesLines(dir);
    assertEquals(lines.length, 2);

    const entities = lines.map((l) => JSON.parse(l) as Record<string, unknown>);
    const entityA = entities.find((e) => e["id"] === CONFLICT_ENTITY_A.id);
    assertEquals(entityA?.["name"], "Updated A"); // incoming wins
  } finally {
    await removeDir(dir);
  }
});

Deno.test("on-conflict merge: properties deep-merged, tags unioned", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath1 = await writeConflictArchive(dir, [CONFLICT_ENTITY_A], []);
    await importArchive(dir, archivePath1);

    const archivePath2 = await writeConflictArchive(
      dir,
      [CONFLICT_ENTITY_A_UPDATED],
      [],
    );
    await importArchive(dir, archivePath2, { onConflict: "merge" });

    const lines = await readEntitiesLines(dir);
    assertEquals(lines.length, 1);

    const entity = JSON.parse(lines[0]) as Record<string, unknown>;
    // Properties should be deep-merged: x from original, y from incoming
    const props = entity["properties"] as Record<string, unknown>;
    assertEquals(props["x"], 1);
    assertEquals(props["y"], 2);
    // Tags should be unioned and sorted
    const tags = entity["tags"] as string[];
    assertEquals(tags.includes("alpha"), true);
    assertEquals(tags.includes("beta"), true);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("on-conflict: bypasses file-level overwrite check", async () => {
  const dir = await makeTempDir();
  try {
    const archivePath1 = await writeConflictArchive(dir, [CONFLICT_ENTITY_A], []);
    await importArchive(dir, archivePath1);

    // Without onConflict and without overwrite, should throw
    const archivePath2 = await writeConflictArchive(
      dir,
      [CONFLICT_ENTITY_NEW],
      [],
      "archive2.json",
    );
    await assertRejects(
      () => importArchive(dir, archivePath2),
      Error,
      "already exists",
    );

    // With onConflict, should succeed without --overwrite
    await importArchive(dir, archivePath2, { onConflict: "skip" });
  } finally {
    await removeDir(dir);
  }
});

Deno.test("runImport: --on-conflict skip is parsed and applied", async () => {
  const dir = await makeTempDir();
  try {
    // Establish existing file
    const archivePath1 = await writeConflictArchive(dir, [CONFLICT_ENTITY_A], []);
    await importArchive(dir, archivePath1);

    // Run via CLI with --on-conflict skip
    const archivePath2 = await writeConflictArchive(
      dir,
      [CONFLICT_ENTITY_A_UPDATED, CONFLICT_ENTITY_NEW],
      [],
      "archive2.json",
    );
    await runImport(dir, ["--on-conflict", "skip", archivePath2]);

    const lines = await readEntitiesLines(dir);
    assertEquals(lines.length, 2);
    const entities = lines.map((l) => JSON.parse(l) as Record<string, unknown>);
    const entityA = entities.find((e) => e["id"] === CONFLICT_ENTITY_A.id);
    assertEquals(entityA?.["name"], "Original A"); // original preserved
  } finally {
    await removeDir(dir);
  }
});
