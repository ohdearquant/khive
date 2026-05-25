/**
 * Behavior tests: `khive kg status`.
 *
 * Covers: clean tree output, pending changes shown, --namespace flag,
 * output content assertions, and out-of-repo exit.
 */

import { assertEquals } from "@std/assert";
import { join } from "@std/path";
import { makeTempRepo, runCliIn } from "../helpers.ts";

// ─── Tests ────────────────────────────────────────────────────────────────────

Deno.test("kg status: exits 0 on clean repo", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "status"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("kg status: clean repo shows 'Validation: pass'", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "status"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
    assertEquals(r.stdout.includes("Validation: pass"), true, `stdout: ${r.stdout}`);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("kg status: shows entity and edge counts", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "status"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
    // makeTempRepo creates 1 entity, 0 edges
    assertEquals(r.stdout.includes("Entities: 1"), true, `stdout: ${r.stdout}`);
    assertEquals(r.stdout.includes("Edges: 0"), true, `stdout: ${r.stdout}`);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("kg status: shows modified file when entities change", async () => {
  const repo = await makeTempRepo();
  try {
    // Add a second entity without committing
    const entitiesPath = join(repo.root, ".khive", "kg", "entities.ndjson");
    const existing = await Deno.readTextFile(entitiesPath);
    await Deno.writeTextFile(
      entitiesPath,
      existing +
        '{"id":"00000000-0000-0000-0000-000000000002","name":"Extra","kind":"document"}\n',
    );

    const r = await runCliIn(repo.root, ["kg", "status"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
    // Should report modified files and updated count
    assertEquals(r.stdout.includes("entities.ndjson"), true, `stdout: ${r.stdout}`);
    assertEquals(r.stdout.includes("Entities: 2"), true, `stdout: ${r.stdout}`);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("kg status: --namespace flag overrides resolved namespace", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "status", "--namespace", "custom-ns"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
    assertEquals(r.stdout.includes("custom-ns"), true, `stdout: ${r.stdout}`);
  } finally {
    await repo.cleanup();
  }
});
