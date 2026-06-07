/**
 * Behavior tests: `khive kg sync`.
 *
 * Covers: exits non-zero when kkernel binary is absent (no kernel installed),
 * exits non-zero outside a git repo, exits 1 on invalid NDJSON, and
 * --quiet suppresses output.
 *
 * Note: kg sync requires the kkernel Rust binary to actually rebuild the
 * working DB. In CI (no binary installed), the command exits 1 with a
 * "kkernel binary not found" error. These tests assert that observable
 * contract — they do NOT test a successful DB rebuild (that requires the
 * binary and is covered by the Rust integration tests).
 */

import { assertEquals } from "@std/assert";
import { join } from "@std/path";
import { makeTempRepo, runCliIn } from "../helpers.ts";

// ─── Tests ────────────────────────────────────────────────────────────────────

// Skipped: this test assumes kkernel binary is NOT findable, but in CI the
// build artifact exists under `crates/target/debug/kkernel` so kernel.ts
// fallback (step 3) resolves successfully. Forcing "not found" requires
// disabling all fallback paths — a follow-up redesign should use a sandboxed
// PATH with no kkernel anywhere, or mock kernel.ts at the import boundary.
Deno.test.ignore(
  "kg sync: exits non-zero when kkernel binary is not installed",
  async () => {
    const repo = await makeTempRepo();
    try {
      const env = { ...Deno.env.toObject(), KKERNEL_BINARY: "" };
      const cliEntry = new URL("../../main.ts", import.meta.url).pathname;
      const cmd = new Deno.Command(Deno.execPath(), {
        args: ["run", "--allow-all", cliEntry, "kg", "sync"],
        cwd: repo.root,
        stdout: "piped",
        stderr: "piped",
        env: { ...env, NO_COLOR: "1" },
      });
      const { code, stderr } = await cmd.output();
      const stderrText = new TextDecoder().decode(stderr);
      assertEquals(code !== 0, true);
      assertEquals(
        stderrText.includes("kkernel") || stderrText.includes("sync"),
        true,
        `Expected kkernel mention in stderr: ${stderrText}`,
      );
    } finally {
      await repo.cleanup();
    }
  },
);

Deno.test("kg sync: exits non-zero outside a git repo", async () => {
  const tmpDir = await Deno.makeTempDir();
  try {
    const r = await runCliIn(tmpDir, ["kg", "sync"]);
    assertEquals(r.code !== 0, true);
  } finally {
    await Deno.remove(tmpDir, { recursive: true });
  }
});

Deno.test("kg sync: exits 1 on invalid NDJSON (validation gate)", async () => {
  const repo = await makeTempRepo();
  try {
    await Deno.writeTextFile(
      join(repo.root, ".khive", "kg", "entities.ndjson"),
      "not valid json\n",
    );
    const r = await runCliIn(repo.root, ["kg", "sync"]);
    assertEquals(r.code, 1, `stdout: ${r.stdout}`);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("kg sync: --quiet flag suppresses stdout on error", async () => {
  const repo = await makeTempRepo();
  try {
    // Invalid NDJSON with --quiet: stdout should be empty
    await Deno.writeTextFile(
      join(repo.root, ".khive", "kg", "entities.ndjson"),
      "not valid json\n",
    );
    const r = await runCliIn(repo.root, ["kg", "sync", "--quiet"]);
    assertEquals(r.code, 1);
    assertEquals(r.stdout.trim().length === 0, true, `Expected empty stdout, got: ${r.stdout}`);
  } finally {
    await repo.cleanup();
  }
});
