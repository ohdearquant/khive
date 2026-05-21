/**
 * Tests for cli/pack/init.ts — `khive pack init` command contract (ADR-050 §4).
 *
 * ADR-050 §4 Authoring specifies:
 *   `khive pack init` (no positional args) creates a `pack.yaml` template in
 *   the current directory. This test file is the regression guard for that
 *   contract.
 */

import { assertEquals, assertStringIncludes } from "@std/assert";
import { join } from "@std/path";
import { runPackInit } from "./init.ts";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Run runPackInit() with the process cwd temporarily set to dir. */
async function runInit(dir: string, args: string[] = []): Promise<number> {
  const original = Deno.cwd();
  Deno.chdir(dir);
  try {
    return await runPackInit(args);
  } finally {
    Deno.chdir(original);
  }
}

// ---------------------------------------------------------------------------
// Contract tests — ADR-050 §4 regression guards
// ---------------------------------------------------------------------------

Deno.test("pack init: no args creates pack.yaml in current directory", async () => {
  const dir = await Deno.makeTempDir({ prefix: "khive_pack_init_test_" });
  try {
    const code = await runInit(dir);
    assertEquals(code, 0, "exit code must be 0 on success");

    const content = await Deno.readTextFile(join(dir, "pack.yaml"));
    assertStringIncludes(content, "name:", "pack.yaml must have a name field");
    assertStringIncludes(
      content,
      "version:",
      "pack.yaml must have a version field",
    );
    assertStringIncludes(
      content,
      "entity_kinds:",
      "pack.yaml must have entity_kinds",
    );
    assertStringIncludes(
      content,
      "note_kinds:",
      "pack.yaml must have note_kinds",
    );
    assertStringIncludes(
      content,
      "edge_endpoints:",
      "pack.yaml must have edge_endpoints",
    );
  } finally {
    await Deno.remove(dir, { recursive: true });
  }
});

Deno.test("pack init: refuses to overwrite existing pack.yaml", async () => {
  const dir = await Deno.makeTempDir({ prefix: "khive_pack_init_test_" });
  try {
    // Pre-create a pack.yaml with custom content.
    const existing = "name: existing\nversion: '9.9.9'\n";
    await Deno.writeTextFile(join(dir, "pack.yaml"), existing);

    const code = await runInit(dir);
    assertEquals(code, 1, "exit code must be 1 when pack.yaml exists");

    // Existing file must be unchanged.
    const content = await Deno.readTextFile(join(dir, "pack.yaml"));
    assertEquals(content, existing, "existing pack.yaml must not be modified");
  } finally {
    await Deno.remove(dir, { recursive: true });
  }
});

Deno.test("pack init: --help exits 0", async () => {
  const code = await runPackInit(["--help"]);
  assertEquals(code, 0, "--help must exit 0");
});

Deno.test("pack init: template includes base kind and relation comments", async () => {
  const dir = await Deno.makeTempDir({ prefix: "khive_pack_init_test_" });
  try {
    await runInit(dir);
    const content = await Deno.readTextFile(join(dir, "pack.yaml"));
    // ADR-050 §4: template includes base entity kinds (ADR-001) and base edge
    // relations (ADR-002) as comments.
    assertStringIncludes(content, "concept", "template must list concept kind");
    assertStringIncludes(
      content,
      "implements",
      "template must list implements relation",
    );
  } finally {
    await Deno.remove(dir, { recursive: true });
  }
});
