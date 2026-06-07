/**
 * Tests for cli/lib/pack.ts — declarative pack manifest validation (ADR-050).
 */

import { assertEquals, assertStringIncludes } from "@std/assert";
import { join } from "@std/path";
import { loadAndValidatePack, validatePackManifest } from "./pack.ts";

// ─── validatePackManifest ───────────────────────────────────────────────────

Deno.test("validatePackManifest — minimal valid manifest", () => {
  const m = {
    name: "ml-papers",
    version: "1.0.0",
  };
  const r = validatePackManifest(m);
  assertEquals(r.valid, true);
  assertEquals(r.errors.length, 0);
});

Deno.test("validatePackManifest — missing name and version", () => {
  const r = validatePackManifest({});
  assertEquals(r.valid, false);
  if (r.errors.length < 2) throw new Error("expected at least 2 errors");
});

Deno.test("validatePackManifest — invalid name format", () => {
  const r = validatePackManifest({ name: "ML-Papers!", version: "1.0.0" });
  assertEquals(r.valid, false);
  assertStringIncludes(r.errors[0].message, "must match");
});

Deno.test("validatePackManifest — invalid semver", () => {
  const r = validatePackManifest({ name: "ok", version: "1.0" });
  assertEquals(r.valid, false);
  assertStringIncludes(r.errors[0].message, "semver");
});

Deno.test("validatePackManifest — entity_kinds with invalid chars", () => {
  const r = validatePackManifest({
    name: "p",
    version: "1.0.0",
    entity_kinds: ["Concept-Bad"],
  });
  assertEquals(r.valid, false);
  assertStringIncludes(r.errors[0].path, "entity_kinds[0]");
});

Deno.test("validatePackManifest — base kind in entity_kinds is a warning, not error", () => {
  const r = validatePackManifest({
    name: "p",
    version: "1.0.0",
    entity_kinds: ["concept"],
  });
  assertEquals(r.valid, true);
  if (r.warnings.length === 0) {
    throw new Error("expected warning about redundant base kind");
  }
  assertStringIncludes(r.warnings[0].message, "redundant");
});

Deno.test("validatePackManifest — edge_endpoint with new relation is an error", () => {
  const r = validatePackManifest({
    name: "p",
    version: "1.0.0",
    edge_endpoints: [
      { relation: "cites", endpoints: [["concept", "concept"]] },
    ],
  });
  assertEquals(r.valid, false);
  assertStringIncludes(r.errors[0].message, "ADR-002");
});

Deno.test("validatePackManifest — edge_endpoint with valid relation passes", () => {
  const r = validatePackManifest({
    name: "p",
    version: "1.0.0",
    edge_endpoints: [
      { relation: "depends_on", endpoints: [["concept", "dataset"]] },
    ],
  });
  assertEquals(r.valid, true);
});

Deno.test("validatePackManifest — edge_endpoint referencing unknown kind warns", () => {
  const r = validatePackManifest({
    name: "p",
    version: "1.0.0",
    edge_endpoints: [
      { relation: "depends_on", endpoints: [["unknown-kind", "concept"]] },
    ],
  });
  assertEquals(r.valid, true); // warning, not error
  if (r.warnings.length === 0) {
    throw new Error("expected warning about unknown kind");
  }
});

Deno.test("validatePackManifest — properties block with invalid key", () => {
  const r = validatePackManifest({
    name: "p",
    version: "1.0.0",
    properties: {
      concept: [{ key: "Bad-Key" }],
    },
  });
  assertEquals(r.valid, false);
  assertStringIncludes(r.errors[0].path, "properties.concept");
});

// ─── loadAndValidatePack ────────────────────────────────────────────────────

Deno.test("loadAndValidatePack — reads pack.yaml from a directory", async () => {
  const dir = await Deno.makeTempDir({ prefix: "khive-pack-" });
  try {
    const yaml = `name: example
version: "1.0.0"
entity_kinds: [model]
edge_endpoints:
  - relation: depends_on
    endpoints:
      - [model, dataset]
`;
    await Deno.writeTextFile(join(dir, "pack.yaml"), yaml);
    const r = await loadAndValidatePack(dir);
    assertEquals(r.valid, true);
    assertEquals(r.manifest?.name, "example");
  } finally {
    await Deno.remove(dir, { recursive: true });
  }
});

Deno.test("loadAndValidatePack — surfaces YAML parse errors", async () => {
  const dir = await Deno.makeTempDir({ prefix: "khive-pack-" });
  try {
    await Deno.writeTextFile(
      join(dir, "pack.yaml"),
      "name: x\nversion: '1.0.0\nentity_kinds: [\n",
    );
    const r = await loadAndValidatePack(dir);
    assertEquals(r.valid, false);
    assertStringIncludes(r.errors[0].message, "parse");
  } finally {
    await Deno.remove(dir, { recursive: true });
  }
});

Deno.test("loadAndValidatePack — missing file surfaces error", async () => {
  const r = await loadAndValidatePack("/nonexistent/path/pack.yaml");
  assertEquals(r.valid, false);
  assertStringIncludes(r.errors[0].message, "cannot read");
});

// ─── Reserved substrate name tests (Critical fix) ──────────────────────────

Deno.test("validatePackManifest — reserved name 'event' in entity_kinds is rejected", () => {
  const r = validatePackManifest({ name: "p", version: "1.0.0", entity_kinds: ["event"] });
  assertEquals(r.valid, false);
  const err = r.errors.find((e) => e.path === "entity_kinds[0]");
  if (!err) throw new Error("expected error for reserved name 'event'");
  assertStringIncludes(err.message, "reserved substrate name");
});

Deno.test("validatePackManifest — legitimate pack-added kind 'model' is accepted", () => {
  const r = validatePackManifest({ name: "p", version: "1.0.0", entity_kinds: ["model"] });
  assertEquals(r.valid, true);
  assertEquals(r.errors.length, 0);
});

Deno.test("validatePackManifest — other reserved names are rejected", () => {
  for (
    const reserved of [
      "note",
      "entity",
      "edge",
      "task",
      "memory",
      "observation",
      "insight",
      "question",
      "decision",
      "reference",
    ]
  ) {
    const r = validatePackManifest({ name: "p", version: "1.0.0", entity_kinds: [reserved] });
    assertEquals(r.valid, false, `expected '${reserved}' to be rejected as reserved`);
    const err = r.errors.find((e) => e.path === "entity_kinds[0]");
    if (!err) throw new Error(`expected reserved-name error for '${reserved}'`);
    assertStringIncludes(err.message, "reserved substrate name");
  }
});

// ─── Unknown top-level key tests (Major 1 fix) ─────────────────────────────

Deno.test("validatePackManifest — verbs field is rejected with specific message", () => {
  const r = validatePackManifest({ name: "p", version: "1.0.0", verbs: ["create"] });
  assertEquals(r.valid, false);
  const err = r.errors.find((e) => e.path === "verbs");
  if (!err) throw new Error("expected error for 'verbs' field");
  assertStringIncludes(err.message, "vocabulary-only");
  assertStringIncludes(err.message, "ADR-050");
});

Deno.test("validatePackManifest — unknown top-level key is rejected", () => {
  const r = validatePackManifest({ name: "p", version: "1.0.0", random_field: "foo" });
  assertEquals(r.valid, false);
  const err = r.errors.find((e) => e.path === "random_field");
  if (!err) throw new Error("expected error for unknown key 'random_field'");
  assertStringIncludes(err.message, "Unknown top-level key");
});

// ─── Duplicate values tests (Major 3 fix) ──────────────────────────────────

Deno.test("validatePackManifest — distinct property values are accepted", () => {
  const r = validatePackManifest({
    name: "p",
    version: "1.0.0",
    entity_kinds: ["model"],
    properties: { model: [{ key: "status", values: ["draft", "published", "archived"] }] },
  });
  assertEquals(r.valid, true);
  assertEquals(r.errors.length, 0);
});

Deno.test("validatePackManifest — duplicate property values are rejected", () => {
  const r = validatePackManifest({
    name: "p",
    version: "1.0.0",
    entity_kinds: ["model"],
    properties: { model: [{ key: "status", values: ["draft", "draft"] }] },
  });
  assertEquals(r.valid, false);
  const err = r.errors.find((e) => e.message.includes("duplicate value"));
  if (!err) throw new Error("expected duplicate-value error");
  assertStringIncludes(err.message, "draft");
});

Deno.test("validatePackManifest — non-adjacent duplicate values are rejected", () => {
  const r = validatePackManifest({
    name: "p",
    version: "1.0.0",
    entity_kinds: ["model"],
    properties: { model: [{ key: "status", values: ["a", "b", "a"] }] },
  });
  assertEquals(r.valid, false);
  const err = r.errors.find((e) => e.message.includes("duplicate value"));
  if (!err) throw new Error("expected duplicate-value error for non-adjacent duplicate");
  assertStringIncludes(err.message, "'a'");
});

// ─── CLI dispatch routing regression (round-3 fix) ─────────────────────────

Deno.test("dispatchPack — 'validate' routes to deferred-phase2, not check", async () => {
  // Spawn the CLI with `pack validate` and assert it exits 1 with the
  // Phase 2 deferral message — NOT the `runPackCheck` path (which would
  // require a valid pack dir argument and produce different output).
  const proc = new Deno.Command(Deno.execPath(), {
    args: [
      "run",
      "--allow-all",
      new URL("../main.ts", import.meta.url).pathname,
      "pack",
      "validate",
      "./nonexistent-pack.yaml",
    ],
    stdout: "piped",
    stderr: "piped",
  });
  const { code, stderr } = await proc.output();
  const stderrText = new TextDecoder().decode(stderr);
  assertEquals(code, 1, "exit code must be 1 for deferred command");
  assertStringIncludes(
    stderrText,
    "deferred to Phase 2",
    "'validate' must hit the deferred-phase2 branch, not runPackCheck",
  );
});
