/**
 * Behavior tests: exit code correctness.
 *
 * Tests that the CLI exits with 0 on success and non-zero on failure.
 */

import { assertEquals } from "@std/assert";
import { makeTempRepo, runCli, runCliIn } from "../helpers.ts";

// ─── Top-level flags ───────────────────────────────────────────────────────────

Deno.test("exit: khive --version exits 0", async () => {
  const r = await runCli(["--version"]);
  assertEquals(r.code, 0);
});

Deno.test("exit: khive -V exits 0", async () => {
  const r = await runCli(["-V"]);
  assertEquals(r.code, 0);
});

Deno.test("exit: khive --help exits 0", async () => {
  const r = await runCli(["--help"]);
  assertEquals(r.code, 0);
});

Deno.test("exit: khive -h exits 0", async () => {
  const r = await runCli(["-h"]);
  assertEquals(r.code, 0);
});

Deno.test("exit: khive (no args) exits 0", async () => {
  const r = await runCli([]);
  assertEquals(r.code, 0);
});

// ─── Unknown command groups ────────────────────────────────────────────────────

Deno.test("exit: unknown top-level command exits 1", async () => {
  const r = await runCli(["unknown-command"]);
  assertEquals(r.code, 1);
});

Deno.test("exit: unknown kg subcommand exits 1", async () => {
  const r = await runCli(["kg", "unknown-subcommand"]);
  assertEquals(r.code, 1);
});

Deno.test("exit: unknown pack subcommand exits 1", async () => {
  const r = await runCli(["pack", "unknown-subcommand"]);
  assertEquals(r.code, 1);
});

Deno.test("exit: auth (removed) exits 1 as unknown group", async () => {
  const r = await runCli(["auth", "login"]);
  assertEquals(r.code, 1);
});

// ─── kg group help ─────────────────────────────────────────────────────────────

Deno.test("exit: khive kg --help exits 0", async () => {
  const r = await runCli(["kg", "--help"]);
  assertEquals(r.code, 0);
});

Deno.test("exit: khive kg (no subcommand) exits 0", async () => {
  const r = await runCli(["kg"]);
  assertEquals(r.code, 0);
});

// ─── pack group ────────────────────────────────────────────────────────────────

Deno.test("exit: khive pack --help exits 0", async () => {
  const r = await runCli(["pack", "--help"]);
  assertEquals(r.code, 0);
});

Deno.test("exit: khive pack (no subcommand) exits 0", async () => {
  const r = await runCli(["pack"]);
  assertEquals(r.code, 0);
});

// ─── kg update stub exits non-zero ────────────────────────────────────────────

Deno.test("exit: khive kg update exits 1 (not implemented)", async () => {
  const r = await runCli(["kg", "update"]);
  assertEquals(r.code, 1);
});

// ─── In-repo commands ─────────────────────────────────────────────────────────

Deno.test("exit: kg validate on valid repo exits 0", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "validate"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("exit: kg stats on valid repo exits 0", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "stats"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("exit: kg doctor on valid repo exits 0", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "doctor"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("exit: kg status on valid repo exits 0", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "status"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
  } finally {
    await repo.cleanup();
  }
});
