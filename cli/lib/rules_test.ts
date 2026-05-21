/**
 * Tests for cli/lib/rules.ts — rule-based validation pass (ADR-056).
 */

import { assertEquals, assertStringIncludes } from "@std/assert";
import { join } from "@std/path";
import { loadRules, runRules } from "./rules.ts";

const SCHEMA = `format_version: "1.0.0"
entity_kinds: [concept, document]
edge_relations:
  - relation: depends_on
    category: dependency
`;

async function withRepo(
  files: Record<string, string>,
  fn: (root: string) => Promise<void>,
): Promise<void> {
  const root = await Deno.makeTempDir({ prefix: "khive-rules-" });
  await Deno.mkdir(join(root, ".khive/kg"), { recursive: true });
  await Deno.writeTextFile(join(root, ".khive/kg/schema.yaml"), SCHEMA);
  for (const [path, content] of Object.entries(files)) {
    const full = join(root, path);
    await Deno.mkdir(full.substring(0, full.lastIndexOf("/")), { recursive: true });
    await Deno.writeTextFile(full, content);
  }
  try {
    await fn(root);
  } finally {
    await Deno.remove(root, { recursive: true });
  }
}

// ─── loadRules ──────────────────────────────────────────────────────────────

Deno.test("loadRules — defaults when rules.yaml absent", async () => {
  await withRepo({}, async (root) => {
    const rules = await loadRules(root);
    const selfLoop = rules.find((r) => r.id === "no-self-loops");
    if (!selfLoop) throw new Error("no-self-loops must be loaded by default");
    assertEquals(selfLoop.severity, "error");
    assertEquals(selfLoop.enabled, true);
  });
});

Deno.test("loadRules — overrides severity from rules.yaml", async () => {
  const rules_yaml = `rules:
  no-self-loops:
    severity: warning
    enabled: true
`;
  await withRepo({ ".khive/kg/rules.yaml": rules_yaml }, async (root) => {
    const rules = await loadRules(root);
    const r = rules.find((x) => x.id === "no-self-loops");
    assertEquals(r?.severity, "warning");
  });
});

Deno.test("loadRules — unknown rule id surfaces as _unknown_rule", async () => {
  const rules_yaml = `rules:
  not-a-real-rule:
    severity: error
`;
  await withRepo({ ".khive/kg/rules.yaml": rules_yaml }, async (root) => {
    const rules = await loadRules(root);
    const unk = rules.find((x) => x.id === "_unknown_rule");
    if (!unk) throw new Error("expected _unknown_rule entry for typo");
  });
});

// ─── runRules: no-self-loops ────────────────────────────────────────────────

Deno.test("runRules — flags self-loop edges", async () => {
  const id = "10000000-0000-0000-0000-000000000001";
  const entities = JSON.stringify({
    id,
    name: "A",
    kind: "concept",
    properties: {},
  }) + "\n";
  const edges = JSON.stringify({
    edge_id: "20000000-0000-0000-0000-000000000002",
    source: id,
    target: id,
    relation: "depends_on",
  }) + "\n";

  await withRepo({
    ".khive/kg/entities.ndjson": entities,
    ".khive/kg/edges.ndjson": edges,
  }, async (root) => {
    const result = await runRules(root);
    const v = result.violations.find((x) => x.rule === "no-self-loops");
    if (!v) throw new Error("expected self-loop violation");
    assertEquals(v.severity, "error");
    assertStringIncludes(v.message, "self-loop");
  });
});

// ─── runRules: no-orphan-entities ───────────────────────────────────────────

Deno.test("runRules — flags orphan entities", async () => {
  const id1 = "10000000-0000-0000-0000-000000000001";
  const id2 = "20000000-0000-0000-0000-000000000002";
  const id3 = "30000000-0000-0000-0000-000000000003";
  const entities = [
    JSON.stringify({ id: id1, name: "A", kind: "concept", properties: {} }),
    JSON.stringify({ id: id2, name: "B", kind: "concept", properties: {} }),
    JSON.stringify({ id: id3, name: "Orphan", kind: "concept", properties: {} }),
  ].join("\n") + "\n";
  const edges = JSON.stringify({
    edge_id: "40000000-0000-0000-0000-000000000004",
    source: id1,
    target: id2,
    relation: "depends_on",
  }) + "\n";
  // Enable orphans rule explicitly (it's default-enabled with min_edges=1).
  const rules_yaml = `rules:
  no-orphan-entities:
    severity: warning
    enabled: true
    config:
      min_edges: 1
`;
  await withRepo({
    ".khive/kg/entities.ndjson": entities,
    ".khive/kg/edges.ndjson": edges,
    ".khive/kg/rules.yaml": rules_yaml,
  }, async (root) => {
    const result = await runRules(root);
    const orphans = result.violations.filter((x) => x.rule === "no-orphan-entities");
    assertEquals(orphans.length, 1);
    assertStringIncludes(orphans[0].message, id3);
  });
});

// ─── runRules: required-properties ──────────────────────────────────────────

Deno.test("runRules — flags missing required properties", async () => {
  const entities = [
    JSON.stringify({
      id: "10000000-0000-0000-0000-000000000001",
      name: "WithDescription",
      kind: "concept",
      properties: { description: "OK" },
    }),
    JSON.stringify({
      id: "20000000-0000-0000-0000-000000000002",
      name: "MissingDescription",
      kind: "concept",
      properties: {},
    }),
  ].join("\n") + "\n";
  const rules_yaml = `rules:
  required-properties:
    severity: error
    enabled: true
    config:
      concept: [description]
`;
  await withRepo({
    ".khive/kg/entities.ndjson": entities,
    ".khive/kg/edges.ndjson": "",
    ".khive/kg/rules.yaml": rules_yaml,
  }, async (root) => {
    const result = await runRules(root);
    const v = result.violations.filter((x) => x.rule === "required-properties");
    assertEquals(v.length, 1);
    assertStringIncludes(v[0].message, "description");
  });
});

// ─── runRules: max-entity-count ─────────────────────────────────────────────

Deno.test("runRules — max-entity-count fires when over the cap", async () => {
  const entities = [
    JSON.stringify({ id: "10000000-0000-0000-0000-000000000001", name: "A", kind: "concept" }),
    JSON.stringify({ id: "20000000-0000-0000-0000-000000000002", name: "B", kind: "concept" }),
  ].join("\n") + "\n";
  const rules_yaml = `rules:
  max-entity-count:
    severity: info
    enabled: true
    config:
      max: 1
      message: "Too many entities"
`;
  await withRepo({
    ".khive/kg/entities.ndjson": entities,
    ".khive/kg/edges.ndjson": "",
    ".khive/kg/rules.yaml": rules_yaml,
  }, async (root) => {
    const result = await runRules(root);
    const v = result.violations.find((x) => x.rule === "max-entity-count");
    if (!v) throw new Error("expected max-entity-count violation");
    assertEquals(v.severity, "info");
    assertStringIncludes(v.message, "Too many");
  });
});

// ─── runRules: disabled rule is skipped ─────────────────────────────────────

Deno.test("runRules — disabled rule is skipped", async () => {
  const entities = JSON.stringify({
    id: "10000000-0000-0000-0000-000000000001",
    name: "X",
    kind: "concept",
    properties: {},
  }) + "\n";
  const edges = JSON.stringify({
    edge_id: "20000000-0000-0000-0000-000000000002",
    source: "10000000-0000-0000-0000-000000000001",
    target: "10000000-0000-0000-0000-000000000001",
    relation: "depends_on",
  }) + "\n";
  const rules_yaml = `rules:
  no-self-loops:
    enabled: false
`;
  await withRepo({
    ".khive/kg/entities.ndjson": entities,
    ".khive/kg/edges.ndjson": edges,
    ".khive/kg/rules.yaml": rules_yaml,
  }, async (root) => {
    const result = await runRules(root);
    const self = result.violations.filter((x) => x.rule === "no-self-loops");
    assertEquals(self.length, 0);
    if (!result.skippedDisabled.includes("no-self-loops")) {
      throw new Error("expected no-self-loops in skippedDisabled");
    }
  });
});
