/**
 * Contract tests: help text golden file comparisons.
 * These tests assert that --help output matches the committed golden files.
 */

import { assertEquals } from "@std/assert";
import { assertGolden, runCli } from "../helpers.ts";
import { join } from "@std/path";

const GOLDEN_DIR = new URL("../golden/", import.meta.url).pathname;

// ─── Top-level help ────────────────────────────────────────────────────────────

Deno.test("help: khive --help exits 0", async () => {
  const r = await runCli(["--help"]);
  assertEquals(r.code, 0);
});

Deno.test("help: khive --help matches golden file", async () => {
  const r = await runCli(["--help"]);
  assertEquals(r.code, 0);
  assertGolden(r.stdout, join(GOLDEN_DIR, "help_toplevel.txt"));
});

Deno.test("help: khive -h exits 0", async () => {
  const r = await runCli(["-h"]);
  assertEquals(r.code, 0);
});

Deno.test("help: no args shows usage (same as --help)", async () => {
  const r = await runCli([]);
  assertEquals(r.code, 0);
  // Should contain usage info
  assertEquals(r.stdout.includes("Usage:"), true);
});

// ─── kg group help ─────────────────────────────────────────────────────────────

Deno.test("help: khive kg --help exits 0", async () => {
  const r = await runCli(["kg", "--help"]);
  assertEquals(r.code, 0);
});

Deno.test("help: khive kg --help matches golden file", async () => {
  const r = await runCli(["kg", "--help"]);
  assertEquals(r.code, 0);
  assertGolden(r.stdout, join(GOLDEN_DIR, "help_kg.txt"));
});

Deno.test("help: khive kg -h exits 0", async () => {
  const r = await runCli(["kg", "-h"]);
  assertEquals(r.code, 0);
});

Deno.test("help: khive kg with no subcommand shows usage", async () => {
  const r = await runCli(["kg"]);
  assertEquals(r.code, 0);
  assertEquals(r.stdout.includes("Usage: khive kg"), true);
});

// ─── pack group help ───────────────────────────────────────────────────────────

Deno.test("help: khive pack --help exits 0", async () => {
  const r = await runCli(["pack", "--help"]);
  assertEquals(r.code, 0);
});

Deno.test("help: khive pack --help matches golden file", async () => {
  const r = await runCli(["pack", "--help"]);
  assertEquals(r.code, 0);
  assertGolden(r.stdout, join(GOLDEN_DIR, "help_pack.txt"));
});

Deno.test("help: khive pack with no subcommand shows usage", async () => {
  const r = await runCli(["pack"]);
  assertEquals(r.code, 0);
  assertEquals(r.stdout.includes("Usage: khive pack"), true);
});

// ─── Content assertions ────────────────────────────────────────────────────────

Deno.test("help: top-level help lists groups (mcp, kg, pack)", async () => {
  const r = await runCli(["--help"]);
  assertEquals(r.code, 0);
  assertEquals(r.stdout.includes("khive mcp"), true);
  assertEquals(r.stdout.includes("khive kg"), true);
  assertEquals(r.stdout.includes("khive pack"), true);
});

Deno.test("help: kg help lists all known subcommands", async () => {
  const r = await runCli(["kg", "--help"]);
  assertEquals(r.code, 0);
  for (
    const sub of [
      "init",
      "validate",
      "commit",
      "sync",
      "status",
      "config",
      "embed",
      "export",
      "import",
      "resolve",
      "hook",
      "migrate",
      "diff",
      "log",
      "stats",
      "doctor",
    ]
  ) {
    assertEquals(
      r.stdout.includes(sub),
      true,
      `Expected kg help to mention '${sub}'`,
    );
  }
});
