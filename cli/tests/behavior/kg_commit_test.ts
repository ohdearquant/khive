/**
 * Behavior tests: `khive kg commit`.
 *
 * Covers: clean-repo no-op, changed files commit, -m flag, out-of-repo rejection,
 * and validation failure aborting the commit.
 */

import { assertEquals } from "@std/assert";
import { join } from "@std/path";
import { makeTempRepo, runCliIn } from "../helpers.ts";

// ─── Helpers ──────────────────────────────────────────────────────────────────

/** Append a second entity to the repo's entities.ndjson. */
async function addEntity(repoRoot: string): Promise<void> {
  const path = join(repoRoot, ".khive", "kg", "entities.ndjson");
  const existing = await Deno.readTextFile(path);
  await Deno.writeTextFile(
    path,
    existing +
      '{"id":"00000000-0000-0000-0000-000000000002","name":"Second","kind":"document"}\n',
  );
}

// ─── Tests ────────────────────────────────────────────────────────────────────

Deno.test("kg commit: nothing to commit on a clean repo exits 0", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "commit", "-m", "no-op"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
    assertEquals(r.stdout.includes("Nothing to commit"), true);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("kg commit: exits 0 when there are staged changes", async () => {
  const repo = await makeTempRepo();
  try {
    await addEntity(repo.root);
    const r = await runCliIn(repo.root, ["kg", "commit", "-m", "add second entity"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("kg commit: -m flag sets commit message (appears in output)", async () => {
  const repo = await makeTempRepo();
  try {
    await addEntity(repo.root);
    const msg = "my-test-commit-message";
    const r = await runCliIn(repo.root, ["kg", "commit", "-m", msg]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
    assertEquals(r.stdout.includes(msg), true);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("kg commit: exits non-zero outside a git repo", async () => {
  // /private/tmp is not a valid git repo (or is but has no .khive/kg)
  const tmpDir = await Deno.makeTempDir();
  try {
    const r = await runCliIn(tmpDir, ["kg", "commit", "-m", "should-fail"]);
    assertEquals(r.code !== 0, true);
  } finally {
    await Deno.remove(tmpDir, { recursive: true });
  }
});

Deno.test("kg commit: aborts and exits non-zero on invalid NDJSON", async () => {
  const repo = await makeTempRepo();
  try {
    // Write invalid NDJSON — breaks validation
    await Deno.writeTextFile(
      join(repo.root, ".khive", "kg", "entities.ndjson"),
      "not valid json at all\n",
    );
    const r = await runCliIn(repo.root, ["kg", "commit", "-m", "bad data"]);
    assertEquals(r.code !== 0, true);
    assertEquals(r.stdout.includes("Commit aborted") || r.stderr.includes("Commit aborted"), true);
  } finally {
    await repo.cleanup();
  }
});
