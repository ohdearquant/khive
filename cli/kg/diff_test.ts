/**
 * Tests for `khive kg diff`.
 */

import { assertEquals } from "@std/assert";
import { computeDiff, diffMaps, parseRecordMap } from "./diff.ts";
import { exec } from "../lib/git.ts";

// ─── Test harness ─────────────────────────────────────────────────────────────

async function makeTempDir(): Promise<string> {
  return await Deno.makeTempDir({ prefix: "khive_diff_test_" });
}

async function removeDir(path: string): Promise<void> {
  await Deno.remove(path, { recursive: true });
}

async function makeGitRepo(): Promise<string> {
  const dir = await makeTempDir();
  await exec(["git", "-C", dir, "init"]);
  await exec(["git", "-C", dir, "config", "user.email", "test@test.com"]);
  await exec(["git", "-C", dir, "config", "user.name", "Test"]);
  return dir;
}

// ─── Unit tests: parseRecordMap ───────────────────────────────────────────────

Deno.test("parseRecordMap: parses entity records by id", () => {
  const text =
    '{"id":"aaaa","name":"A","kind":"concept"}\n{"id":"bbbb","name":"B","kind":"project"}\n';
  const map = parseRecordMap(text, "id");
  assertEquals(map.size, 2);
  assertEquals((map.get("aaaa") as Record<string, unknown>)["name"], "A");
  assertEquals((map.get("bbbb") as Record<string, unknown>)["name"], "B");
});

Deno.test("parseRecordMap: parses edge records by edge_id", () => {
  const text = '{"edge_id":"e1","source":"a","target":"b","relation":"contains"}\n';
  const map = parseRecordMap(text, "edge_id");
  assertEquals(map.size, 1);
  assertEquals(map.has("e1"), true);
});

Deno.test("parseRecordMap: skips blank lines", () => {
  const text = '\n\n{"id":"x","name":"X","kind":"concept"}\n\n';
  const map = parseRecordMap(text, "id");
  assertEquals(map.size, 1);
});

Deno.test("parseRecordMap: skips comment lines", () => {
  const text = '# comment\n{"id":"y","name":"Y","kind":"concept"}\n';
  const map = parseRecordMap(text, "id");
  assertEquals(map.size, 1);
});

Deno.test("parseRecordMap: skips invalid JSON lines", () => {
  const text = 'not-json\n{"id":"z","name":"Z","kind":"concept"}\n';
  const map = parseRecordMap(text, "id");
  assertEquals(map.size, 1);
});

Deno.test("parseRecordMap: skips records with missing key field", () => {
  const text = '{"name":"No id","kind":"concept"}\n{"id":"ok","name":"OK","kind":"concept"}\n';
  const map = parseRecordMap(text, "id");
  assertEquals(map.size, 1);
  assertEquals(map.has("ok"), true);
});

Deno.test("parseRecordMap: empty string returns empty map", () => {
  assertEquals(parseRecordMap("", "id").size, 0);
  assertEquals(parseRecordMap("\n\n", "id").size, 0);
});

// ─── Unit tests: diffMaps ─────────────────────────────────────────────────────

Deno.test("diffMaps: two empty maps produce no changes", () => {
  const changes = diffMaps(new Map(), new Map(), "entity");
  assertEquals(changes.length, 0);
});

Deno.test("diffMaps: added record", () => {
  const base = new Map<string, Record<string, unknown>>();
  const head = new Map<string, Record<string, unknown>>([
    ["a1", { id: "a1", name: "A", kind: "concept" }],
  ]);
  const changes = diffMaps(base, head, "entity");
  assertEquals(changes.length, 1);
  assertEquals(changes[0].change, "added");
  assertEquals(changes[0].id, "a1");
  assertEquals(changes[0].kind, "entity");
  assertEquals(changes[0].fields.length, 0);
});

Deno.test("diffMaps: removed record", () => {
  const base = new Map<string, Record<string, unknown>>([
    ["a1", { id: "a1", name: "A" }],
  ]);
  const head = new Map<string, Record<string, unknown>>();
  const changes = diffMaps(base, head, "entity");
  assertEquals(changes.length, 1);
  assertEquals(changes[0].change, "removed");
  assertEquals(changes[0].id, "a1");
  assertEquals(changes[0].before, { id: "a1", name: "A" });
  assertEquals(changes[0].after, undefined);
});

Deno.test("diffMaps: modified record lists changed fields", () => {
  const base = new Map<string, Record<string, unknown>>([
    ["a1", { id: "a1", name: "Old", kind: "concept" }],
  ]);
  const head = new Map<string, Record<string, unknown>>([
    ["a1", { id: "a1", name: "New", kind: "concept" }],
  ]);
  const changes = diffMaps(base, head, "entity");
  assertEquals(changes.length, 1);
  assertEquals(changes[0].change, "modified");
  assertEquals(changes[0].fields, ["name"]);
});

Deno.test("diffMaps: unchanged record produces no change entry", () => {
  const rec = { id: "a1", name: "A", kind: "concept" };
  const base = new Map<string, Record<string, unknown>>([["a1", rec]]);
  const head = new Map<string, Record<string, unknown>>([["a1", { ...rec }]]);
  const changes = diffMaps(base, head, "entity");
  assertEquals(changes.length, 0);
});

Deno.test("diffMaps: edge changes use 'edge' kind", () => {
  const base = new Map<string, Record<string, unknown>>();
  const head = new Map<string, Record<string, unknown>>([
    ["e1", { edge_id: "e1", source: "a", target: "b", relation: "contains" }],
  ]);
  const changes = diffMaps(base, head, "edge");
  assertEquals(changes.length, 1);
  assertEquals(changes[0].kind, "edge");
});

Deno.test("diffMaps: field added to a record is detected as modification", () => {
  const base = new Map<string, Record<string, unknown>>([["a1", { id: "a1" }]]);
  const head = new Map<string, Record<string, unknown>>([["a1", { id: "a1", name: "New" }]]);
  const changes = diffMaps(base, head, "entity");
  assertEquals(changes.length, 1);
  assertEquals(changes[0].change, "modified");
  assertEquals(changes[0].fields.includes("name"), true);
});

// ─── Integration tests: computeDiff ──────────────────────────────────────────

Deno.test("computeDiff: no KG files produces empty diff", async () => {
  const dir = await makeTempDir();
  try {
    const diff = await computeDiff(dir, []);
    assertEquals(diff.changes.length, 0);
    assertEquals(diff.base, "HEAD");
    assertEquals(diff.head, "working tree");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeDiff: working tree addition detected", async () => {
  const dir = await makeGitRepo();
  try {
    await Deno.mkdir(`${dir}/.khive/kg`, { recursive: true });
    const entity = {
      id: "aaaaaaaa-0000-0000-0000-000000000001",
      name: "A",
      kind: "concept",
    };
    await Deno.writeTextFile(
      `${dir}/.khive/kg/entities.ndjson`,
      JSON.stringify(entity) + "\n",
    );
    await Deno.writeTextFile(`${dir}/.khive/kg/edges.ndjson`, "");
    await exec(["git", "-C", dir, "add", "."]);
    await exec(["git", "-C", dir, "commit", "-m", "init"]);

    // Add entity B to working tree
    const entity2 = {
      id: "bbbbbbbb-0000-0000-0000-000000000002",
      name: "B",
      kind: "project",
    };
    await Deno.writeTextFile(
      `${dir}/.khive/kg/entities.ndjson`,
      JSON.stringify(entity) + "\n" + JSON.stringify(entity2) + "\n",
    );

    const diff = await computeDiff(dir, []);
    const added = diff.changes.filter((c) => c.change === "added");
    assertEquals(added.length, 1);
    assertEquals(added[0].id, entity2.id);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeDiff: working tree removal detected", async () => {
  const dir = await makeGitRepo();
  try {
    await Deno.mkdir(`${dir}/.khive/kg`, { recursive: true });
    const entity = {
      id: "aaaaaaaa-0000-0000-0000-000000000001",
      name: "A",
      kind: "concept",
    };
    await Deno.writeTextFile(
      `${dir}/.khive/kg/entities.ndjson`,
      JSON.stringify(entity) + "\n",
    );
    await Deno.writeTextFile(`${dir}/.khive/kg/edges.ndjson`, "");
    await exec(["git", "-C", dir, "add", "."]);
    await exec(["git", "-C", dir, "commit", "-m", "init"]);

    // Remove entity from working tree
    await Deno.writeTextFile(`${dir}/.khive/kg/entities.ndjson`, "");

    const diff = await computeDiff(dir, []);
    const removed = diff.changes.filter((c) => c.change === "removed");
    assertEquals(removed.length, 1);
    assertEquals(removed[0].id, entity.id);
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeDiff: two git refs compares committed states", async () => {
  const dir = await makeGitRepo();
  try {
    await Deno.mkdir(`${dir}/.khive/kg`, { recursive: true });

    // Commit 1: empty
    await Deno.writeTextFile(`${dir}/.khive/kg/entities.ndjson`, "");
    await Deno.writeTextFile(`${dir}/.khive/kg/edges.ndjson`, "");
    await exec(["git", "-C", dir, "add", "."]);
    await exec(["git", "-C", dir, "commit", "-m", "empty"]);

    // Commit 2: with entity
    const entity = {
      id: "aaaaaaaa-0000-0000-0000-000000000001",
      name: "A",
      kind: "concept",
    };
    await Deno.writeTextFile(
      `${dir}/.khive/kg/entities.ndjson`,
      JSON.stringify(entity) + "\n",
    );
    await exec(["git", "-C", dir, "add", "."]);
    await exec(["git", "-C", dir, "commit", "-m", "add entity"]);

    const diff = await computeDiff(dir, ["HEAD~1", "HEAD"]);
    const added = diff.changes.filter((c) => c.change === "added");
    assertEquals(added.length, 1);
    assertEquals(added[0].id, entity.id);
    assertEquals(diff.base, "HEAD~1");
    assertEquals(diff.head, "HEAD");
  } finally {
    await removeDir(dir);
  }
});

Deno.test("computeDiff: --name-only flag is parsed", async () => {
  const dir = await makeTempDir();
  try {
    const diff = await computeDiff(dir, ["--name-only"]);
    assertEquals(diff.changes.length, 0);
  } finally {
    await removeDir(dir);
  }
});
