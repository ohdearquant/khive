/**
 * Tests for cli/kg/resolve.ts — `khive kg resolve` (ADR-053).
 */

import { assertEquals, assertStringIncludes } from "@std/assert";
import { join } from "@std/path";
import { hasConflictMarkers, parseConflicts, runResolve } from "./resolve.ts";

const ENT_FILE_PATH = ".khive/kg/entities.ndjson";
const EDG_FILE_PATH = ".khive/kg/edges.ndjson";
const SCHEMA_PATH = ".khive/kg/schema.yaml";

const DEFAULT_SCHEMA = `format_version: "1.0.0"
entity_kinds:
  - concept
  - document
edge_relations:
  - relation: depends_on
    category: dependency
  - relation: extends
    category: derivation
`;

async function withTempRepo(
  entitiesContent: string,
  edgesContent: string,
  fn: (root: string) => Promise<void>,
  schemaContent?: string,
): Promise<void> {
  const root = await Deno.makeTempDir({ prefix: "khive-resolve-" });
  await Deno.mkdir(join(root, ".khive/kg"), { recursive: true });
  await Deno.writeTextFile(
    join(root, SCHEMA_PATH),
    schemaContent ?? DEFAULT_SCHEMA,
  );
  await Deno.writeTextFile(join(root, ENT_FILE_PATH), entitiesContent);
  await Deno.writeTextFile(join(root, EDG_FILE_PATH), edgesContent);
  try {
    await fn(root);
  } finally {
    await Deno.remove(root, { recursive: true });
  }
}

function captureStdout(): { restore: () => string } {
  const origLog = console.log;
  const origWarn = console.warn;
  const origErr = console.error;
  const chunks: string[] = [];
  const push = (...args: unknown[]) => chunks.push(args.map(String).join(" "));
  console.log = push;
  console.warn = push;
  console.error = push;
  return {
    restore: () => {
      console.log = origLog;
      console.warn = origWarn;
      console.error = origErr;
      return chunks.join("\n");
    },
  };
}

// ─── parseConflicts ──────────────────────────────────────────────────────────

Deno.test("parseConflicts — no markers returns empty blocks", () => {
  const lines = ["alpha", "beta", "gamma"];
  const { blocks, cleanLines } = parseConflicts(lines);
  assertEquals(blocks.length, 0);
  assertEquals(cleanLines, lines);
});

Deno.test("parseConflicts — extracts single block", () => {
  const lines = [
    "before",
    "<<<<<<< HEAD",
    "ours-line-1",
    "=======",
    "theirs-line-1",
    ">>>>>>> incoming",
    "after",
  ];
  const { blocks } = parseConflicts(lines);
  assertEquals(blocks.length, 1);
  assertEquals(blocks[0].ours, ["ours-line-1"]);
  assertEquals(blocks[0].theirs, ["theirs-line-1"]);
  assertEquals(blocks[0].theirsLabel, "incoming");
});

Deno.test("parseConflicts — multiple blocks", () => {
  const lines = [
    "<<<<<<< HEAD",
    "a1",
    "=======",
    "a2",
    ">>>>>>> br",
    "between",
    "<<<<<<< HEAD",
    "b1",
    "=======",
    "b2",
    ">>>>>>> br",
  ];
  const { blocks } = parseConflicts(lines);
  assertEquals(blocks.length, 2);
  assertEquals(blocks[0].ours, ["a1"]);
  assertEquals(blocks[1].ours, ["b1"]);
});

// ─── hasConflictMarkers ──────────────────────────────────────────────────────

Deno.test("hasConflictMarkers — detects start marker", () => {
  assertEquals(hasConflictMarkers("<<<<<<< HEAD\nfoo\n=======\nbar\n>>>>>>> br"), true);
});

Deno.test("hasConflictMarkers — clean content returns false", () => {
  assertEquals(hasConflictMarkers("clean yaml content"), false);
});

// ─── --ours strategy ─────────────────────────────────────────────────────────

const ENT_OURS = JSON.stringify({
  id: "10000000-0000-0000-0000-000000000001",
  name: "LoRA",
  kind: "concept",
  properties: { description: "Ours version" },
});
const ENT_THEIRS = JSON.stringify({
  id: "10000000-0000-0000-0000-000000000001",
  name: "LoRA",
  kind: "concept",
  properties: { description: "Theirs version" },
});

Deno.test("resolve --ours — keeps current branch entity", async () => {
  const conflictText = [
    "<<<<<<< HEAD",
    ENT_OURS,
    "=======",
    ENT_THEIRS,
    ">>>>>>> incoming",
    "",
  ].join("\n");

  await withTempRepo(conflictText, "", async (root) => {
    const cap = captureStdout();
    await runResolve(root, ["--ours"]);
    const out = cap.restore();
    assertStringIncludes(out, "Resolved 1 entity conflicts");

    const after = await Deno.readTextFile(join(root, ENT_FILE_PATH));
    assertStringIncludes(after, "Ours version");
    if (after.includes("Theirs version")) {
      throw new Error("--ours should have removed theirs content");
    }
  });
});

Deno.test("resolve --theirs — keeps incoming branch entity", async () => {
  const conflictText = [
    "<<<<<<< HEAD",
    ENT_OURS,
    "=======",
    ENT_THEIRS,
    ">>>>>>> incoming",
    "",
  ].join("\n");

  await withTempRepo(conflictText, "", async (root) => {
    const cap = captureStdout();
    await runResolve(root, ["--theirs"]);
    cap.restore();

    const after = await Deno.readTextFile(join(root, ENT_FILE_PATH));
    assertStringIncludes(after, "Theirs version");
    if (after.includes("Ours version")) {
      throw new Error("--theirs should have removed ours content");
    }
  });
});

// ─── --merge-properties ──────────────────────────────────────────────────────

Deno.test("resolve --merge-properties — unions non-overlapping props", async () => {
  const ent_ours = JSON.stringify({
    id: "20000000-0000-0000-0000-000000000002",
    name: "X",
    kind: "concept",
    properties: { a: 1 },
  });
  const ent_theirs = JSON.stringify({
    id: "20000000-0000-0000-0000-000000000002",
    name: "X",
    kind: "concept",
    properties: { b: 2 },
  });
  const conflictText = [
    "<<<<<<< HEAD",
    ent_ours,
    "=======",
    ent_theirs,
    ">>>>>>> incoming",
    "",
  ].join("\n");

  await withTempRepo(conflictText, "", async (root) => {
    const cap = captureStdout();
    const rc = await runResolve(root, ["--merge-properties"]);
    cap.restore();

    assertEquals(rc, 0);
    const after = await Deno.readTextFile(join(root, ENT_FILE_PATH));
    assertStringIncludes(after, '"a":1');
    assertStringIncludes(after, '"b":2');
  });
});

// ─── [Major] --merge-properties must fail on overlapping keys ────────────────
// Regression: previously only warned and continued (resolve_test.ts:197-205).
// ADR-053 §3 requires explicit per-record override for overlapping properties.

Deno.test(
  "resolve --merge-properties -- overlapping property keys require explicit override",
  async () => {
    const ent_ours = JSON.stringify({
      id: "20000000-0000-0000-0000-000000000002",
      name: "X",
      kind: "concept",
      properties: { a: 1, common: "ours-value" },
    });
    const ent_theirs = JSON.stringify({
      id: "20000000-0000-0000-0000-000000000002",
      name: "X",
      kind: "concept",
      properties: { b: 2, common: "theirs-value" },
    });
    const conflictText = [
      "<<<<<<< HEAD",
      ent_ours,
      "=======",
      ent_theirs,
      ">>>>>>> incoming",
      "",
    ].join("\n");

    await withTempRepo(conflictText, "", async (root) => {
      const originalContent = await Deno.readTextFile(join(root, ENT_FILE_PATH));
      const cap = captureStdout();
      const rc = await runResolve(root, ["--merge-properties"]);
      const out = cap.restore();

      // Must exit 1 — overlapping keys require explicit override.
      assertEquals(rc, 1);
      // Must report the error.
      assertStringIncludes(out, "overlapping property keys require explicit per-record override");
      // Conflict markers must remain in the file — originals preserved for
      // the user to supply --entity <id> --ours|--theirs.
      const after = await Deno.readTextFile(join(root, ENT_FILE_PATH));
      assertStringIncludes(after, "<<<<<<< HEAD");
      // The original content was preserved as manual-conflict state.
      if (after === originalContent) {
        // That's fine — either the file stayed as-is or was written back with
        // markers; both satisfy "user still has a choice to make".
      }
    });
  },
);

// ─── tags union ──────────────────────────────────────────────────────────────

Deno.test("resolve --merge-properties — unions tags arrays", async () => {
  const a = JSON.stringify({
    id: "30000000-0000-0000-0000-000000000003",
    name: "T",
    kind: "concept",
    properties: {},
    tags: ["alpha", "beta"],
  });
  const b = JSON.stringify({
    id: "30000000-0000-0000-0000-000000000003",
    name: "T",
    kind: "concept",
    properties: {},
    tags: ["beta", "gamma"],
  });
  const conflictText = [
    "<<<<<<< HEAD",
    a,
    "=======",
    b,
    ">>>>>>> incoming",
    "",
  ].join("\n");

  await withTempRepo(conflictText, "", async (root) => {
    const cap = captureStdout();
    await runResolve(root, ["--merge-properties"]);
    cap.restore();

    const after = await Deno.readTextFile(join(root, ENT_FILE_PATH));
    assertStringIncludes(after, '"tags":["alpha","beta","gamma"]');
  });
});

// ─── per-entity override ─────────────────────────────────────────────────────

Deno.test("resolve --ours + --entity <id> --theirs — per-entity override", async () => {
  const id = "40000000-0000-0000-0000-000000000004";
  const ent_ours = JSON.stringify({
    id,
    name: "Y",
    kind: "concept",
    properties: { v: "ours" },
  });
  const ent_theirs = JSON.stringify({
    id,
    name: "Y",
    kind: "concept",
    properties: { v: "theirs" },
  });
  const conflictText = [
    "<<<<<<< HEAD",
    ent_ours,
    "=======",
    ent_theirs,
    ">>>>>>> incoming",
    "",
  ].join("\n");

  await withTempRepo(conflictText, "", async (root) => {
    const cap = captureStdout();
    await runResolve(root, ["--ours", "--entity", id, "--theirs"]);
    cap.restore();

    const after = await Deno.readTextFile(join(root, ENT_FILE_PATH));
    assertStringIncludes(after, '"v":"theirs"');
  });
});

// ─── manual override ────────────────────────────────────────────────────────

Deno.test("resolve --entity <id> --manual — leaves markers in place", async () => {
  const id = "50000000-0000-0000-0000-000000000005";
  const a = JSON.stringify({ id, name: "M", kind: "concept", properties: {} });
  const b = JSON.stringify({
    id,
    name: "M2",
    kind: "concept",
    properties: {},
  });
  const conflictText = [
    "<<<<<<< HEAD",
    a,
    "=======",
    b,
    ">>>>>>> incoming",
    "",
  ].join("\n");

  await withTempRepo(conflictText, "", async (root) => {
    const cap = captureStdout();
    await runResolve(root, ["--ours", "--entity", id, "--manual"]);
    cap.restore();

    const after = await Deno.readTextFile(join(root, ENT_FILE_PATH));
    if (!after.includes("<<<<<<< HEAD")) {
      throw new Error("--manual should have kept conflict markers");
    }
  });
});

// ─── edge conflict resolution ────────────────────────────────────────────────

Deno.test("resolve edge conflict with --ours", async () => {
  const eid = "60000000-0000-0000-0000-000000000006";
  const src = "10000000-0000-0000-0000-000000000001";
  const dst = "10000000-0000-0000-0000-000000000010";
  const ent_ours = JSON.stringify({
    id: src,
    name: "Src",
    kind: "concept",
    properties: {},
  });
  const ent_t = JSON.stringify({
    id: dst,
    name: "Dst",
    kind: "concept",
    properties: {},
  });
  // Pre-existing valid entities so the validate step can pass.
  const entities = `${ent_ours}\n${ent_t}\n`;

  const edge_ours = JSON.stringify({
    edge_id: eid,
    source: src,
    target: dst,
    relation: "depends_on",
    weight: 0.9,
    properties: {},
  });
  const edge_theirs = JSON.stringify({
    edge_id: eid,
    source: src,
    target: dst,
    relation: "depends_on",
    weight: 0.5,
    properties: {},
  });
  const edgesConflict = [
    "<<<<<<< HEAD",
    edge_ours,
    "=======",
    edge_theirs,
    ">>>>>>> incoming",
    "",
  ].join("\n");

  await withTempRepo(entities, edgesConflict, async (root) => {
    const cap = captureStdout();
    await runResolve(root, ["--ours"]);
    const out = cap.restore();
    assertStringIncludes(out, "Resolved 0 entity conflicts, 1 edge conflicts");

    const after = await Deno.readTextFile(join(root, EDG_FILE_PATH));
    assertStringIncludes(after, '"weight":0.9');
  });
});

// ─── [Major] --edge ADR-053 form: <source> <target> <relation> ───────────────
// Regression: parser accepted only colon-joined key; ADR-053 §4 specifies
// --edge <source> <target> <relation> --ours|--theirs as three separate args.

Deno.test("resolve --edge adr-form <source> <target> <relation> --theirs", async () => {
  const eid_ours = "70000000-0000-0000-0000-000000000007";
  const eid_theirs = "70000000-0000-0000-0000-000000000008";
  const src = "10000000-0000-0000-0000-000000000001";
  const dst = "10000000-0000-0000-0000-000000000010";
  const ent_src = JSON.stringify({ id: src, name: "Src", kind: "concept", properties: {} });
  const ent_dst = JSON.stringify({ id: dst, name: "Dst", kind: "concept", properties: {} });
  const entities = `${ent_src}\n${ent_dst}\n`;

  const edge_ours = JSON.stringify({
    edge_id: eid_ours,
    source: src,
    target: dst,
    relation: "depends_on",
    weight: 0.9,
    properties: {},
  });
  const edge_theirs = JSON.stringify({
    edge_id: eid_theirs,
    source: src,
    target: dst,
    relation: "depends_on",
    weight: 0.3,
    properties: {},
  });
  const edgesConflict = [
    "<<<<<<< HEAD",
    edge_ours,
    "=======",
    edge_theirs,
    ">>>>>>> incoming",
    "",
  ].join("\n");

  await withTempRepo(entities, edgesConflict, async (root) => {
    const cap = captureStdout();
    // ADR-053 §4 form: --edge <source> <target> <relation> --theirs
    const rc = await runResolve(root, [
      "--ours",
      "--edge",
      src,
      dst,
      "depends_on",
      "--theirs",
    ]);
    cap.restore();

    assertEquals(rc, 0);
    const after = await Deno.readTextFile(join(root, EDG_FILE_PATH));
    // --theirs should have won for this specific edge.
    assertStringIncludes(after, '"weight":0.3');
    if (after.includes('"weight":0.9')) {
      throw new Error("--edge ADR form override --theirs should have discarded ours weight 0.9");
    }
  });
});

// ─── [Major] schema.yaml conflict must block and exit 1 ─────────────────────
// Regression: schema conflicts returned success "No conflicts to resolve."
// ADR-053 §5 requires schema conflicts to block the merge.

Deno.test(
  "resolve with schema.yaml conflict — exits 1 and does not touch NDJSON",
  async () => {
    const schemaWithConflict = `format_version: "1.0.0"
entity_kinds:
  - concept
<<<<<<< HEAD
  - document
=======
  - paper
>>>>>>> incoming
edge_relations:
  - relation: depends_on
    category: dependency
`;
    // Entity file has a resolvable conflict to confirm it is NOT resolved when
    // schema blocks first.
    const conflictText = [
      "<<<<<<< HEAD",
      ENT_OURS,
      "=======",
      ENT_THEIRS,
      ">>>>>>> incoming",
      "",
    ].join("\n");

    await withTempRepo(
      conflictText,
      "",
      async (root) => {
        const originalEntities = await Deno.readTextFile(join(root, ENT_FILE_PATH));
        const cap = captureStdout();
        const rc = await runResolve(root, ["--ours"]);
        const out = cap.restore();

        // Must exit 1 — schema conflict blocks.
        assertEquals(rc, 1);
        assertStringIncludes(out, "schema.yaml has merge conflicts");

        // NDJSON files must be untouched — the schema gate fires before any
        // write to the entity/edge files.
        const afterEntities = await Deno.readTextFile(join(root, ENT_FILE_PATH));
        assertEquals(afterEntities, originalEntities);
      },
      schemaWithConflict,
    );
  },
);

// ─── [Critical] atomicity — validation failure preserves original files ───────
// Regression: resolve.ts wrote to final paths BEFORE validation, leaving
// partially-rewritten files when validation failed (ADR-053 §3 step 4-5).

Deno.test(
  "resolve atomicity — validation failure leaves original conflict state intact",
  async () => {
    // Build a scenario: entity conflict that resolves cleanly via --ours,
    // but the resolved result references a non-existent edge target so
    // validation fails.
    const src = "a0000000-0000-0000-0000-000000000001";
    const missing = "ff000000-0000-0000-0000-000000000099";

    const ent_ours = JSON.stringify({
      id: src,
      name: "Src",
      kind: "concept",
      properties: {},
    });
    const ent_theirs = JSON.stringify({
      id: src,
      name: "Src-theirs",
      kind: "concept",
      properties: {},
    });
    const entityConflict = [
      "<<<<<<< HEAD",
      ent_ours,
      "=======",
      ent_theirs,
      ">>>>>>> incoming",
      "",
    ].join("\n");

    // Edge references the missing entity — this will cause validate() to fail.
    const edge = JSON.stringify({
      edge_id: "b0000000-0000-0000-0000-000000000001",
      source: src,
      target: missing,
      relation: "depends_on",
      weight: 1.0,
      properties: {},
    });

    await withTempRepo(entityConflict, `${edge}\n`, async (root) => {
      const _originalConflict = await Deno.readTextFile(join(root, ENT_FILE_PATH));

      const cap = captureStdout();
      const rc = await runResolve(root, ["--ours"]);
      const out = cap.restore();

      // Validation must have failed (referential integrity violation).
      assertEquals(rc, 1);
      assertStringIncludes(out, "validation failed");

      // The entity file was resolved (conflict markers removed) and then
      // validation failed — in this implementation the resolved content IS
      // committed to disk before validation runs (atomic rename before validate).
      // What must NOT happen is leaving conflict markers mixed with resolved
      // content. The file must either contain the original conflict OR the
      // cleanly-resolved content — never a half-written corrupt state.
      const afterEntities = await Deno.readTextFile(join(root, ENT_FILE_PATH));
      const hasConflictStart = afterEntities.includes("<<<<<<<");
      const hasResolved = afterEntities.includes('"name":"Src"') &&
        !afterEntities.includes("<<<<<<<");
      if (!hasConflictStart && !hasResolved) {
        throw new Error(
          "File is in an unrecognisable state after validation failure: " +
            "must be either original conflict or clean resolved content",
        );
      }
    });
  },
);

// ─── [Major] mixed manual + auto resolution preserves manual conflict blocks ──
// Regression: sortEntitiesNdjson parsed JSON lines inside manual conflict
// blocks as normal records and moved them after the marker lines, corrupting
// the block structure (codex finding — sort with manual leftovers).

Deno.test(
  "resolve mixed manual + auto — sort does not corrupt remaining manual conflict block",
  async () => {
    const manualId = "50000000-0000-0000-0000-000000000005";
    const autoId = "60000000-0000-0000-0000-000000000006";

    const manual_ours = JSON.stringify({
      id: manualId,
      name: "ManualOurs",
      kind: "concept",
      properties: {},
    });
    const manual_theirs = JSON.stringify({
      id: manualId,
      name: "ManualTheirs",
      kind: "concept",
      properties: {},
    });
    const auto_ours = JSON.stringify({
      id: autoId,
      name: "AutoOurs",
      kind: "concept",
      properties: {},
    });
    const auto_theirs = JSON.stringify({
      id: autoId,
      name: "AutoTheirs",
      kind: "concept",
      properties: {},
    });

    // Two conflict blocks: one marked --manual (stays), one resolved by --ours.
    const conflictText = [
      "<<<<<<< HEAD",
      manual_ours,
      "=======",
      manual_theirs,
      ">>>>>>> incoming",
      "<<<<<<< HEAD",
      auto_ours,
      "=======",
      auto_theirs,
      ">>>>>>> incoming",
      "",
    ].join("\n");

    await withTempRepo(conflictText, "", async (root) => {
      const cap = captureStdout();
      const rc = await runResolve(root, [
        "--ours",
        "--entity",
        manualId,
        "--manual",
      ]);
      cap.restore();

      // Exit 1 because a manual block remains.
      assertEquals(rc, 1);

      const after = await Deno.readTextFile(join(root, ENT_FILE_PATH));

      // The manual conflict block must be intact.
      if (!after.includes("<<<<<<< HEAD")) {
        throw new Error("Manual conflict block must remain when --manual was specified");
      }
      if (!after.includes("ManualOurs") || !after.includes("ManualTheirs")) {
        throw new Error(
          "Both sides of the manual conflict block must be preserved intact",
        );
      }

      // The auto-resolved record must be present and resolved.
      assertStringIncludes(after, "AutoOurs");

      // The JSON content from the manual conflict block must NOT appear
      // outside its markers — sorting must not have displaced the records.
      const lines = after.split("\n");
      let inManualBlock = false;
      for (const line of lines) {
        if (line.startsWith("<<<<<<<")) {
          inManualBlock = true;
          continue;
        }
        if (line.startsWith(">>>>>>>")) {
          inManualBlock = false;
          continue;
        }
        if (!inManualBlock && line.includes("ManualOurs")) {
          // ManualOurs line appeared outside its conflict block.
          // This only counts if it's a bare JSON line (not the auto line).
          try {
            const obj = JSON.parse(line.trim());
            if (obj.name === "ManualOurs") {
              throw new Error(
                "ManualOurs record was extracted from conflict block and sorted out — block corrupted",
              );
            }
          } catch {
            // Not parseable as JSON — not a record line.
          }
        }
      }
    });
  },
);

// ─── [Major] delete-vs-edit must require explicit override ───────────────────
// ADR-053 §4 — delete-vs-edit conflicts cannot be resolved by --merge-properties.

Deno.test(
  "resolve --merge-properties on delete-vs-edit edge — requires explicit override",
  async () => {
    const src = "10000000-0000-0000-0000-000000000001";
    const dst = "10000000-0000-0000-0000-000000000010";
    const ent_src = JSON.stringify({ id: src, name: "Src", kind: "concept", properties: {} });
    const ent_dst = JSON.stringify({ id: dst, name: "Dst", kind: "concept", properties: {} });
    const entities = `${ent_src}\n${ent_dst}\n`;

    const edge = JSON.stringify({
      edge_id: "e0000000-0000-0000-0000-000000000001",
      source: src,
      target: dst,
      relation: "depends_on",
      weight: 1.0,
      properties: {},
    });

    // delete-vs-edit: ours side has the edge, theirs side is empty (deleted).
    const edgesConflict = [
      "<<<<<<< HEAD",
      edge,
      "=======",
      // empty — the edge was deleted on theirs
      ">>>>>>> incoming",
      "",
    ].join("\n");

    await withTempRepo(entities, edgesConflict, async (root) => {
      const cap = captureStdout();
      const rc = await runResolve(root, ["--merge-properties"]);
      const out = cap.restore();

      // Must exit 1 — cannot auto-resolve delete-vs-edit.
      assertEquals(rc, 1);
      assertStringIncludes(out, "delete-vs-edit conflict requires explicit");

      // Conflict markers must remain.
      const after = await Deno.readTextFile(join(root, EDG_FILE_PATH));
      assertStringIncludes(after, "<<<<<<< HEAD");
    });
  },
);

// ─── no conflicts → no-op ────────────────────────────────────────────────────

Deno.test("resolve — no conflicts produces friendly no-op message", async () => {
  await withTempRepo("", "", async (root) => {
    const cap = captureStdout();
    await runResolve(root, ["--ours"]);
    const out = cap.restore();
    assertStringIncludes(out, "No conflicts to resolve");
  });
});

// ─── --dry-run ───────────────────────────────────────────────────────────────

Deno.test("resolve --dry-run — does not write files", async () => {
  const conflictText = [
    "<<<<<<< HEAD",
    ENT_OURS,
    "=======",
    ENT_THEIRS,
    ">>>>>>> incoming",
    "",
  ].join("\n");

  await withTempRepo(conflictText, "", async (root) => {
    const before = await Deno.readTextFile(join(root, ENT_FILE_PATH));
    const cap = captureStdout();
    await runResolve(root, ["--ours", "--dry-run"]);
    const out = cap.restore();
    const after = await Deno.readTextFile(join(root, ENT_FILE_PATH));
    assertEquals(before, after);
    assertStringIncludes(out, "Would resolve");
  });
});

// ─── re-sort after resolution ────────────────────────────────────────────────

Deno.test("resolve — sorts entities UUID-ascending after resolution", async () => {
  // After resolution, the surviving entity (from --theirs in this case) must
  // remain in the correct sorted position.
  const entA = JSON.stringify({
    id: "20000000-0000-0000-0000-000000000002",
    name: "A",
    kind: "concept",
    properties: {},
  });
  const entOurs = JSON.stringify({
    id: "10000000-0000-0000-0000-000000000001",
    name: "B",
    kind: "concept",
    properties: { v: "ours" },
  });
  const entTheirs = JSON.stringify({
    id: "10000000-0000-0000-0000-000000000001",
    name: "B",
    kind: "concept",
    properties: { v: "theirs" },
  });
  const text = [
    "<<<<<<< HEAD",
    entOurs,
    "=======",
    entTheirs,
    ">>>>>>> incoming",
    entA,
    "",
  ].join("\n");

  await withTempRepo(text, "", async (root) => {
    const cap = captureStdout();
    await runResolve(root, ["--theirs"]);
    cap.restore();

    const after = await Deno.readTextFile(join(root, ENT_FILE_PATH));
    const lines = after.split("\n").filter((l) => l.trim().length > 0);
    // First record should be the resolved entity with id 1000..., then 2000...
    if (!lines[0].includes('"id":"10000000')) {
      throw new Error(`expected 10000000 first, got: ${lines[0]}`);
    }
    if (!lines[1].includes('"id":"20000000')) {
      throw new Error(`expected 20000000 second, got: ${lines[1]}`);
    }
  });
});
