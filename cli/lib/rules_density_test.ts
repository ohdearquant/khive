/**
 * Tests for min-edge-density rule — positive and negative cases.
 * Coverage gap identified in codex round-1 High finding.
 */

import { assertEquals, assertStringIncludes } from "@std/assert";
import { join } from "@std/path";
import { runRules } from "./rules.ts";

const SCHEMA = `format_version: "1.0.0"
entity_kinds:
  - concept
  - person
edge_relations:
  - relation: depends_on
`;

async function withRepo(
  files: Record<string, string>,
  fn: (root: string) => Promise<void>,
): Promise<void> {
  const root = await Deno.makeTempDir({ prefix: "khive-density-" });
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

const ID1 = "10000000-0000-0000-0000-000000000001";
const ID2 = "20000000-0000-0000-0000-000000000002";
const ID3 = "30000000-0000-0000-0000-000000000003";

// ─── Positive case: density above threshold ───────────────────────────────────

Deno.test("runRules min-edge-density — no violation when density meets threshold", async () => {
  const entities = [
    JSON.stringify({ id: ID1, name: "A", kind: "concept", properties: {} }),
    JSON.stringify({ id: ID2, name: "B", kind: "concept", properties: {} }),
  ].join("\n") + "\n";
  const edges = [
    JSON.stringify({
      edge_id: "40000000-0000-0000-0000-000000000004",
      source: ID1,
      target: ID2,
      relation: "depends_on",
    }),
    JSON.stringify({
      edge_id: "50000000-0000-0000-0000-000000000005",
      source: ID2,
      target: ID1,
      relation: "depends_on",
    }),
  ].join("\n") + "\n";

  const rulesYaml = `rules:
  min-edge-density:
    severity: warning
    enabled: true
    config:
      min_edges_per_entity: 1
`;

  await withRepo({
    ".khive/kg/entities.ndjson": entities,
    ".khive/kg/edges.ndjson": edges,
    ".khive/kg/rules.yaml": rulesYaml,
  }, async (root) => {
    const result = await runRules(root);
    const v = result.violations.filter((x) => x.rule === "min-edge-density");
    assertEquals(v.length, 0, "no density violation expected when density >= threshold");
  });
});

// ─── Negative case: density below threshold ───────────────────────────────────

Deno.test("runRules min-edge-density — violation when density below threshold", async () => {
  const entities = [
    JSON.stringify({ id: ID1, name: "A", kind: "concept", properties: {} }),
    JSON.stringify({ id: ID2, name: "B", kind: "concept", properties: {} }),
    JSON.stringify({ id: ID3, name: "C", kind: "concept", properties: {} }),
  ].join("\n") + "\n";
  // Only 1 edge for 3 entities → avg 0.33, below threshold of 2.
  const edges = JSON.stringify({
    edge_id: "40000000-0000-0000-0000-000000000004",
    source: ID1,
    target: ID2,
    relation: "depends_on",
  }) + "\n";

  const rulesYaml = `rules:
  min-edge-density:
    severity: warning
    enabled: true
    config:
      min_edges_per_entity: 2
`;

  await withRepo({
    ".khive/kg/entities.ndjson": entities,
    ".khive/kg/edges.ndjson": edges,
    ".khive/kg/rules.yaml": rulesYaml,
  }, async (root) => {
    const result = await runRules(root);
    const v = result.violations.filter((x) => x.rule === "min-edge-density");
    assertEquals(v.length, 1);
    assertStringIncludes(v[0].message, "below target");
    assertEquals(v[0].severity, "warning");
  });
});

// ─── exclude_kinds removes entities from denominator ─────────────────────────

Deno.test("runRules min-edge-density — exclude_kinds removes kind from denominator", async () => {
  // 2 concept entities (no edges), 1 person entity.
  // With persons excluded → 0 edges / 2 concepts = density 0, below threshold 1.
  // Without exclude: 0/3 = 0, still below threshold.
  // Test confirms exclusion is applied when threshold would otherwise be met.
  const entities = [
    JSON.stringify({ id: ID1, name: "A", kind: "concept", properties: {} }),
    JSON.stringify({ id: ID2, name: "B", kind: "concept", properties: {} }),
    JSON.stringify({ id: ID3, name: "Researcher", kind: "person", properties: {} }),
  ].join("\n") + "\n";

  // 2 edges connecting person to concepts. With person excluded:
  // denominator = 2 (concepts), edges = 2 → avg = 1.0 ≥ threshold 1.
  const edges = [
    JSON.stringify({
      edge_id: "40000000-0000-0000-0000-000000000004",
      source: ID1,
      target: ID2,
      relation: "depends_on",
    }),
    JSON.stringify({
      edge_id: "50000000-0000-0000-0000-000000000005",
      source: ID2,
      target: ID1,
      relation: "depends_on",
    }),
  ].join("\n") + "\n";

  const rulesYamlExclude = `rules:
  min-edge-density:
    severity: warning
    enabled: true
    config:
      min_edges_per_entity: 1
      exclude_kinds: [person]
`;

  await withRepo({
    ".khive/kg/entities.ndjson": entities,
    ".khive/kg/edges.ndjson": edges,
    ".khive/kg/rules.yaml": rulesYamlExclude,
  }, async (root) => {
    const result = await runRules(root);
    const v = result.violations.filter((x) => x.rule === "min-edge-density");
    assertEquals(v.length, 0, "person excluded — density of concepts meets threshold");
  });
});

// ─── Empty graph does not trigger density rule ────────────────────────────────

Deno.test("runRules min-edge-density — empty graph does not fire (no entities)", async () => {
  const rulesYaml = `rules:
  min-edge-density:
    severity: warning
    enabled: true
    config:
      min_edges_per_entity: 5
`;

  await withRepo({
    ".khive/kg/entities.ndjson": "",
    ".khive/kg/edges.ndjson": "",
    ".khive/kg/rules.yaml": rulesYaml,
  }, async (root) => {
    const result = await runRules(root);
    const v = result.violations.filter((x) => x.rule === "min-edge-density");
    assertEquals(v.length, 0);
  });
});
