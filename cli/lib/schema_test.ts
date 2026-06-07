/**
 * Tests for schema.yaml parser — specifically the ADR-037 remotes format
 * (list of {name, url, ref, namespace, pin?} entries).
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

// ─── remotes (ADR-037 shape: {name, url, ref, namespace, pin?}) ──────────────

Deno.test("loadSchema: parses ADR-037 remotes as list of {name, url, ref, namespace}", async () => {
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
        "    url: https://github.com/ohdearquant/lattice.git",
        "    ref: main",
        "    namespace: lattice",
        "  - name: atlas",
        "    url: https://github.com/ohdearquant/atlas.git",
        "    ref: main",
        "    namespace: atlas",
        "    pin: sha256:a1b2c3d4e5f6789012345678901234567890abcdef1234567890abcdef12345678",
      ].join("\n") + "\n",
    );

    const schema = await loadSchema(dir);
    assertEquals(Array.isArray(schema.remotes), true);
    assertEquals(schema.remotes!.length, 2);

    const lattice = schema.remotes![0];
    assertEquals(lattice.name, "lattice");
    assertEquals(lattice.url, "https://github.com/ohdearquant/lattice.git");
    assertEquals(lattice.ref, "main");
    assertEquals(lattice.namespace, "lattice");
    assertEquals(lattice.pin, undefined);

    const atlas = schema.remotes![1];
    assertEquals(atlas.name, "atlas");
    assertEquals(atlas.url, "https://github.com/ohdearquant/atlas.git");
    assertEquals(atlas.ref, "main");
    assertEquals(atlas.namespace, "atlas");
    assertEquals(
      atlas.pin,
      "sha256:a1b2c3d4e5f6789012345678901234567890abcdef1234567890abcdef12345678",
    );
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
