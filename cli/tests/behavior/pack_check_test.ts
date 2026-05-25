/**
 * Behavior tests: `khive pack check`.
 *
 * Covers: valid manifest exits 0 with "Validation: pass", invalid manifest
 * exits 1 with "Validation: fail", no-args shows help and exits 1, and
 * nonexistent path exits 1.
 */

import { assertEquals } from "@std/assert";
import { runCli } from "../helpers.ts";

// ─── Helpers ──────────────────────────────────────────────────────────────────

/** Minimal valid pack.yaml content. */
const VALID_PACK_YAML = `name: test-pack
version: "1.0.0"
description: "A test pack"
entity_kinds:
  - research_item
`;

/** pack.yaml with missing required 'name' field. */
const INVALID_PACK_YAML_MISSING_NAME = `version: "1.0.0"
description: "Missing name"
`;

/** pack.yaml with an invalid relation (not in ADR-002 closed set). */
const INVALID_PACK_YAML_BAD_RELATION = `name: bad-pack
version: "1.0.0"
edge_endpoints:
  - relation: invented-relation
    endpoints:
      - [concept, document]
`;

// ─── Tests ────────────────────────────────────────────────────────────────────

Deno.test("pack check: valid manifest exits 0", async () => {
  const tmpDir = await Deno.makeTempDir({ prefix: "khive_packtest_" });
  try {
    const packPath = `${tmpDir}/pack.yaml`;
    await Deno.writeTextFile(packPath, VALID_PACK_YAML);
    const r = await runCli(["pack", "check", packPath]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}\nstdout: ${r.stdout}`);
  } finally {
    await Deno.remove(tmpDir, { recursive: true });
  }
});

Deno.test("pack check: valid manifest outputs 'Validation: pass'", async () => {
  const tmpDir = await Deno.makeTempDir({ prefix: "khive_packtest_" });
  try {
    const packPath = `${tmpDir}/pack.yaml`;
    await Deno.writeTextFile(packPath, VALID_PACK_YAML);
    const r = await runCli(["pack", "check", packPath]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
    assertEquals(r.stdout.includes("Validation: pass"), true, `stdout: ${r.stdout}`);
  } finally {
    await Deno.remove(tmpDir, { recursive: true });
  }
});

Deno.test("pack check: invalid manifest (missing name) exits 1", async () => {
  const tmpDir = await Deno.makeTempDir({ prefix: "khive_packtest_" });
  try {
    const packPath = `${tmpDir}/pack.yaml`;
    await Deno.writeTextFile(packPath, INVALID_PACK_YAML_MISSING_NAME);
    const r = await runCli(["pack", "check", packPath]);
    assertEquals(r.code, 1, `stdout: ${r.stdout}`);
    assertEquals(r.stderr.includes("Validation: fail"), true, `stderr: ${r.stderr}`);
  } finally {
    await Deno.remove(tmpDir, { recursive: true });
  }
});

Deno.test("pack check: invalid manifest (bad edge relation) exits 1 with error detail", async () => {
  const tmpDir = await Deno.makeTempDir({ prefix: "khive_packtest_" });
  try {
    const packPath = `${tmpDir}/pack.yaml`;
    await Deno.writeTextFile(packPath, INVALID_PACK_YAML_BAD_RELATION);
    const r = await runCli(["pack", "check", packPath]);
    assertEquals(r.code, 1, `stdout: ${r.stdout}`);
    // Should mention the offending relation name
    assertEquals(
      r.stderr.includes("invented-relation"),
      true,
      `Expected error for invented-relation in stderr: ${r.stderr}`,
    );
  } finally {
    await Deno.remove(tmpDir, { recursive: true });
  }
});

Deno.test("pack check: no args exits 1 and prints usage", async () => {
  const r = await runCli(["pack", "check"]);
  assertEquals(r.code, 1);
  // Help text includes the command name
  assertEquals(r.stdout.includes("pack check"), true, `stdout: ${r.stdout}`);
});

Deno.test("pack check: accepts directory path (looks for pack.yaml inside)", async () => {
  const tmpDir = await Deno.makeTempDir({ prefix: "khive_packtest_" });
  try {
    // Write pack.yaml inside the dir, then pass the dir as the path
    await Deno.writeTextFile(`${tmpDir}/pack.yaml`, VALID_PACK_YAML);
    const r = await runCli(["pack", "check", tmpDir]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}\nstdout: ${r.stdout}`);
    assertEquals(r.stdout.includes("Validation: pass"), true, `stdout: ${r.stdout}`);
  } finally {
    await Deno.remove(tmpDir, { recursive: true });
  }
});
