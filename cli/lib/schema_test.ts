/**
 * Tests for schema.yaml parser — specifically the ADR-048 remotes format
 * (list of {name, repo, path, commit} entries).
 */

import { assertEquals } from "@std/assert";
import { join } from "@std/path";
import { loadSchema } from "./schema.ts";

async function makeTempDir(): Promise<string> {
  return await Deno.makeTempDir({ prefix: "khive_schema_test_" });
}

async function removeDir(path: string): Promise<void> {
  await Deno.remove(path, { recursive: true });
}

async function writeSchema(dir: string, content: string): Promise<string> {
  await Deno.mkdir(join(dir, ".khive/kg"), { recursive: true });
  const path = join(dir, ".khive/kg/schema.yaml");
  await Deno.writeTextFile(path, content);
  return dir;
}

// ─── remotes (ADR-048 §3 format) ─────────────────────────────────────────────

Deno.test("loadSchema: parses ADR-048 remotes as list of {name, repo, path, commit}", async () => {
  const dir = await makeTempDir();
  try {
    await writeSchema(
      dir,
      [
        'format_version: "1.0.0"',
        "entity_kinds:",
        "  - concept",
        "edge_relations:",
        "  - relation: implements",
        "remotes:",
        "  - name: lattice",
        "    repo: ohdearquant/lattice",
        "    path: .khive/kg",
        "    commit: a1b2c3d4e5f6789012345678901234567890abcd",
        "  - name: atlas",
        "    repo: ohdearquant/atlas",
        "    path: .khive/kg",
        "    commit: f9e8d7c6b5a4321098765432109876543210fedc",
      ].join("\n") + "\n",
    );

    const schema = await loadSchema(dir);
    assertEquals(Array.isArray(schema.remotes), true);
    assertEquals(schema.remotes!.length, 2);

    const lattice = schema.remotes![0];
    assertEquals(lattice.name, "lattice");
    assertEquals(lattice.repo, "ohdearquant/lattice");
    assertEquals(lattice.path, ".khive/kg");
    assertEquals(lattice.commit, "a1b2c3d4e5f6789012345678901234567890abcd");

    const atlas = schema.remotes![1];
    assertEquals(atlas.name, "atlas");
    assertEquals(atlas.repo, "ohdearquant/atlas");
    assertEquals(atlas.path, ".khive/kg");
    assertEquals(atlas.commit, "f9e8d7c6b5a4321098765432109876543210fedc");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("loadSchema: remotes is undefined when section absent", async () => {
  const dir = await makeTempDir();
  try {
    await writeSchema(
      dir,
      [
        'format_version: "1.0.0"',
        "entity_kinds:",
        "  - concept",
        "edge_relations:",
        "  - relation: implements",
      ].join("\n") + "\n",
    );

    const schema = await loadSchema(dir);
    assertEquals(schema.remotes, undefined);
  } finally {
    await removeDir(dir);
  }
});

// ─── entity_kinds and edge_relations ─────────────────────────────────────────

Deno.test("loadSchema: parses entity_kinds list", async () => {
  const dir = await makeTempDir();
  try {
    await writeSchema(
      dir,
      [
        'format_version: "1.0.0"',
        "entity_kinds:",
        "  - concept",
        "  - project",
        "  - person",
        "edge_relations:",
        "  - relation: implements",
      ].join("\n") + "\n",
    );
    const schema = await loadSchema(dir);
    assertEquals(schema.entity_kinds, ["concept", "project", "person"]);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("loadSchema: parses edge_relations with description", async () => {
  const dir = await makeTempDir();
  try {
    await writeSchema(
      dir,
      [
        'format_version: "1.0.0"',
        "entity_kinds:",
        "  - concept",
        "edge_relations:",
        "  - relation: implements",
        '    description: "Code realizes algorithm"',
        "  - relation: depends_on",
      ].join("\n") + "\n",
    );
    const schema = await loadSchema(dir);
    assertEquals(schema.edge_relations.length, 2);
    assertEquals(schema.edge_relations[0].relation, "implements");
    assertEquals(schema.edge_relations[0].description, "Code realizes algorithm");
    assertEquals(schema.edge_relations[1].relation, "depends_on");
    assertEquals(schema.edge_relations[1].description, undefined);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("loadSchema: parses format_version", async () => {
  const dir = await makeTempDir();
  try {
    await writeSchema(
      dir,
      [
        'format_version: "1.0.0"',
        "entity_kinds:",
        "  - concept",
        "edge_relations:",
        "  - relation: implements",
      ].join("\n") + "\n",
    );
    const schema = await loadSchema(dir);
    assertEquals(schema.format_version, "1.0.0");
  } finally {
    await removeDir(dir);
  }
});
