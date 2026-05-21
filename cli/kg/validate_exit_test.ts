/**
 * Tests for runValidate() exit codes and --strict / --no-rules / --format json.
 *
 * Covers High findings from codex round-1 review of PR #134:
 *   - Exit code 2 for malformed rules.yaml (Blocker)
 *   - JSON output matches ADR-056 contract (Medium)
 *   - --strict raises exit code for warnings
 *   - --no-rules skips rule pass
 */

import { assertEquals } from "@std/assert";
import { join } from "@std/path";
import { runValidate } from "./validate.ts";

const VALID_SCHEMA = `format_version: "1.0.0"
entity_kinds:
  - concept
  - document
edge_relations:
  - relation: depends_on
`;

async function withRepo(
  files: Record<string, string>,
  fn: (root: string) => Promise<void>,
): Promise<void> {
  const root = await Deno.makeTempDir({ prefix: "khive-validate-exit-" });
  await Deno.mkdir(join(root, ".khive/kg"), { recursive: true });
  await Deno.writeTextFile(join(root, ".khive/kg/schema.yaml"), VALID_SCHEMA);
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

const ENTITY1_LINE = JSON.stringify({ id: ID1, name: "A", kind: "concept", properties: {} });
const ENTITY2_LINE = JSON.stringify({ id: ID2, name: "B", kind: "concept", properties: {} });
const VALID_ENTITIES = ENTITY1_LINE + "\n" + ENTITY2_LINE + "\n";

const EDGE_LINE = JSON.stringify({
  edge_id: "30000000-0000-0000-0000-000000000003",
  source: ID1,
  target: ID2,
  relation: "depends_on",
});
const VALID_EDGES = EDGE_LINE + "\n";

// ─── Exit code 0: clean graph ─────────────────────────────────────────────────

Deno.test("runValidate — exit code 0 on clean graph (no rules.yaml)", async () => {
  await withRepo({
    ".khive/kg/entities.ndjson": VALID_ENTITIES,
    ".khive/kg/edges.ndjson": VALID_EDGES,
  }, async (root) => {
    const code = await runValidate(root, ["--quiet"]);
    assertEquals(code, 0);
  });
});

// ─── Exit code 1: KG violation ────────────────────────────────────────────────

Deno.test("runValidate — exit code 1 on rule error violation", async () => {
  // Self-loop = error severity by default.
  const selfLoopEdge = JSON.stringify({
    edge_id: "40000000-0000-0000-0000-000000000004",
    source: ID1,
    target: ID1,
    relation: "depends_on",
  });
  await withRepo({
    ".khive/kg/entities.ndjson": ENTITY1_LINE + "\n",
    ".khive/kg/edges.ndjson": selfLoopEdge + "\n",
  }, async (root) => {
    const code = await runValidate(root, ["--quiet"]);
    assertEquals(code, 1);
  });
});

// ─── Exit code 2: malformed rules.yaml ───────────────────────────────────────

Deno.test("runValidate — exit code 2 on rules.yaml with invalid severity (blocker repro)", async () => {
  const badRules = `rules:
  no-self-loops:
    severity: fatal
    enabled: true
`;
  await withRepo({
    ".khive/kg/entities.ndjson": VALID_ENTITIES,
    ".khive/kg/edges.ndjson": VALID_EDGES,
    ".khive/kg/rules.yaml": badRules,
  }, async (root) => {
    const code = await runValidate(root, ["--quiet"]);
    assertEquals(code, 2);
  });
});

Deno.test("runValidate — exit code 2 on rules.yaml with invented entity kind (high finding repro)", async () => {
  // 'paper' is not in ADR-001 — must produce schema error (exit 2), not pass.
  const badRules = `rules:
  required-properties:
    severity: error
    enabled: true
    config:
      paper:
        - title
`;
  await withRepo({
    ".khive/kg/entities.ndjson": VALID_ENTITIES,
    ".khive/kg/edges.ndjson": VALID_EDGES,
    ".khive/kg/rules.yaml": badRules,
  }, async (root) => {
    const code = await runValidate(root, ["--quiet"]);
    assertEquals(code, 2);
  });
});

Deno.test("runValidate — exit code 2 on rules.yaml with unknown top-level key", async () => {
  const badRules = `rules:
  no-self-loops:
    severity: error
extra_key: not_allowed
`;
  await withRepo({
    ".khive/kg/entities.ndjson": VALID_ENTITIES,
    ".khive/kg/edges.ndjson": VALID_EDGES,
    ".khive/kg/rules.yaml": badRules,
  }, async (root) => {
    const code = await runValidate(root, ["--quiet"]);
    assertEquals(code, 2);
  });
});

// ─── --strict: warning becomes exit 1 ────────────────────────────────────────

Deno.test("runValidate -- --strict makes warning-severity violations exit 1", async () => {
  // no-orphan-entities is warning by default; ID2 has no edges.
  const orphanEntities = ENTITY1_LINE + "\n" + ENTITY2_LINE + "\n";
  const singleEdge = EDGE_LINE + "\n"; // only ID1 and ID2 connected from source side

  const rulesYaml = `rules:
  no-orphan-entities:
    severity: warning
    enabled: true
    config:
      min_edges: 2
`;

  await withRepo({
    ".khive/kg/entities.ndjson": orphanEntities,
    ".khive/kg/edges.ndjson": singleEdge,
    ".khive/kg/rules.yaml": rulesYaml,
  }, async (root) => {
    const codeNoStrict = await runValidate(root, ["--quiet"]);
    // Without --strict, warnings should NOT block.
    assertEquals(codeNoStrict, 0);

    const codeStrict = await runValidate(root, ["--quiet", "--strict"]);
    // With --strict, warning violations become blocking.
    assertEquals(codeStrict, 1);
  });
});

// ─── --no-rules: skip rule pass ──────────────────────────────────────────────

Deno.test("runValidate — --no-rules skips rule pass, structural errors still surface", async () => {
  // Self-loop = rule error; with --no-rules it should be skipped → exit 0.
  const selfLoopEdge = JSON.stringify({
    edge_id: "40000000-0000-0000-0000-000000000004",
    source: ID1,
    target: ID1,
    relation: "depends_on",
  });
  await withRepo({
    ".khive/kg/entities.ndjson": ENTITY1_LINE + "\n",
    ".khive/kg/edges.ndjson": selfLoopEdge + "\n",
  }, async (root) => {
    const code = await runValidate(root, ["--no-rules", "--quiet"]);
    assertEquals(code, 0);
  });
});

Deno.test("runValidate — --no-rules still blocks on structural errors", async () => {
  // Duplicate entity ID is a structural error, not a rule.
  const dupEntities = ENTITY1_LINE + "\n" + ENTITY1_LINE + "\n"; // duplicate
  await withRepo({
    ".khive/kg/entities.ndjson": dupEntities,
    ".khive/kg/edges.ndjson": "",
  }, async (root) => {
    const code = await runValidate(root, ["--no-rules", "--quiet"]);
    assertEquals(code, 1);
  });
});

// ─── JSON output shape (ADR-056 §4 contract) ─────────────────────────────────

Deno.test("runValidate — --format json matches ADR-056 shape (rules[] + summary)", async () => {
  // Capture stdout.
  let output = "";
  const originalLog = console.log;
  console.log = (...args: unknown[]) => {
    output += args.join(" ") + "\n";
  };
  try {
    await withRepo({
      ".khive/kg/entities.ndjson": VALID_ENTITIES,
      ".khive/kg/edges.ndjson": VALID_EDGES,
    }, async (root) => {
      await runValidate(root, ["--format=json"]);
    });
  } finally {
    console.log = originalLog;
  }

  const parsed = JSON.parse(output.trim());

  // Must have top-level 'rules' array and 'summary' object.
  if (!Array.isArray(parsed.rules)) throw new Error("expected 'rules' array in JSON output");
  if (typeof parsed.summary !== "object") {
    throw new Error("expected 'summary' object in JSON output");
  }

  // summary must have errors, warnings, info, entities, edges, passed.
  for (const key of ["errors", "warnings", "info", "entities", "edges", "passed"]) {
    if (!(key in parsed.summary)) throw new Error(`summary missing key '${key}'`);
  }

  // Each rule entry must have id, severity, passed, violations.
  for (const rule of parsed.rules) {
    for (const key of ["id", "severity", "passed", "violations"]) {
      if (!(key in rule)) throw new Error(`rule entry missing key '${key}'`);
    }
  }

  assertEquals(parsed.summary.passed, true);
  assertEquals(parsed.summary.errors, 0);
});

Deno.test("runValidate — JSON output 'passed' mirrors --strict setting", async () => {
  // With self-loop (error severity) the JSON passed=false even without --strict.
  const selfLoopEdge = JSON.stringify({
    edge_id: "40000000-0000-0000-0000-000000000004",
    source: ID1,
    target: ID1,
    relation: "depends_on",
  });

  let output = "";
  const originalLog = console.log;
  console.log = (...args: unknown[]) => {
    output += args.join(" ") + "\n";
  };
  try {
    await withRepo({
      ".khive/kg/entities.ndjson": ENTITY1_LINE + "\n",
      ".khive/kg/edges.ndjson": selfLoopEdge + "\n",
    }, async (root) => {
      const code = await runValidate(root, ["--format=json"]);
      assertEquals(code, 1);
    });
  } finally {
    console.log = originalLog;
  }

  const parsed = JSON.parse(output.trim());
  assertEquals(parsed.summary.passed, false);
});
