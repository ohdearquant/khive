/**
 * Behavior tests: error message correctness.
 *
 * Tests that the CLI emits helpful error messages for bad input.
 */

import { assertEquals } from "@std/assert";
import { makeTempRepo, runCli, runCliIn } from "../helpers.ts";
import { join } from "@std/path";

// ─── Unknown commands ──────────────────────────────────────────────────────────

Deno.test("error: unknown top-level command prints error to stderr", async () => {
  const r = await runCli(["badcommand"]);
  assertEquals(r.code, 1);
  assertEquals(r.stderr.includes("Unknown command group"), true);
});

Deno.test("error: unknown top-level command suggests --help", async () => {
  const r = await runCli(["badcommand"]);
  assertEquals(r.code, 1);
  assertEquals(r.stderr.includes("--help"), true);
});

Deno.test("error: unknown kg subcommand prints error to stderr", async () => {
  const r = await runCli(["kg", "badcommand"]);
  assertEquals(r.code, 1);
  assertEquals(r.stderr.includes("Unknown kg subcommand"), true);
});

Deno.test("error: unknown kg subcommand suggests 'khive kg --help'", async () => {
  const r = await runCli(["kg", "badcommand"]);
  assertEquals(r.code, 1);
  assertEquals(r.stderr.includes("khive kg --help"), true);
});

Deno.test("error: unknown pack subcommand prints error to stderr", async () => {
  const r = await runCli(["pack", "badcommand"]);
  assertEquals(r.code, 1);
  assertEquals(r.stderr.includes("Unknown pack subcommand"), true);
});

Deno.test("error: unknown pack subcommand suggests 'khive pack --help'", async () => {
  const r = await runCli(["pack", "badcommand"]);
  assertEquals(r.code, 1);
  assertEquals(r.stderr.includes("khive pack --help"), true);
});

Deno.test("error: auth is now an unknown command group", async () => {
  const r = await runCli(["auth", "login"]);
  assertEquals(r.code, 1);
  assertEquals(r.stderr.includes("Unknown command group"), true);
});

Deno.test("error: kg update shows not-implemented message", async () => {
  const r = await runCli(["kg", "update"]);
  assertEquals(r.code, 1);
  assertEquals(r.stderr.includes("not yet implemented"), true);
});

// ─── pack check with bad path ──────────────────────────────────────────────────

Deno.test("error: pack check nonexistent.yaml exits non-zero", async () => {
  const r = await runCli(["pack", "check", "nonexistent-file-that-does-not-exist.yaml"]);
  assertEquals(r.code !== 0, true);
});

// ─── Out-of-repo kg commands ───────────────────────────────────────────────────

Deno.test("error: kg validate outside git repo prints error", async () => {
  // /tmp is not a git repo
  const r = await runCliIn("/tmp", ["kg", "validate"]);
  assertEquals(r.code !== 0, true);
});

Deno.test("error: kg stats outside git repo prints error", async () => {
  const r = await runCliIn("/tmp", ["kg", "stats"]);
  assertEquals(r.code !== 0, true);
});

// ─── Invalid NDJSON ────────────────────────────────────────────────────────────

Deno.test("error: kg validate on invalid NDJSON exits non-zero", async () => {
  const repo = await makeTempRepo();
  try {
    // Write invalid NDJSON to entities file
    const entitiesPath = join(repo.root, ".khive", "kg", "entities.ndjson");
    await Deno.writeTextFile(entitiesPath, "not valid json\n");
    const r = await runCliIn(repo.root, ["kg", "validate"]);
    assertEquals(r.code !== 0, true);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("error: kg validate on invalid entity (missing kind) exits non-zero", async () => {
  const repo = await makeTempRepo();
  try {
    const entitiesPath = join(repo.root, ".khive", "kg", "entities.ndjson");
    await Deno.writeTextFile(
      entitiesPath,
      '{"id":"ent_00000000-0000-0000-0000-000000000001","name":"bad"}\n',
    );
    const r = await runCliIn(repo.root, ["kg", "validate"]);
    assertEquals(r.code !== 0, true);
  } finally {
    await repo.cleanup();
  }
});
