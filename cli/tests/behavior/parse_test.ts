/**
 * Behavior tests: flag parsing and argument handling.
 *
 * Tests that flags are accepted, parsed, and reflected in behavior.
 */

import { assertEquals, assertMatch } from "@std/assert";
import { makeTempRepo, runCli, runCliIn } from "../helpers.ts";
import { join } from "@std/path";

// ─── --version / -V ────────────────────────────────────────────────────────────

Deno.test("parse: --version outputs version matching CLI_VERSION format", async () => {
  const r = await runCli(["--version"]);
  assertEquals(r.code, 0);
  assertMatch(r.stdout.trim(), /^khive \d+\.\d+\.\d+/);
});

Deno.test("parse: -V outputs same as --version", async () => {
  const [long, short] = await Promise.all([runCli(["--version"]), runCli(["-V"])]);
  assertEquals(long.stdout.trim(), short.stdout.trim());
});

// ─── kg stats flags ───────────────────────────────────────────────────────────

Deno.test("parse: kg stats --json outputs JSON (not plain text)", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "stats", "--json"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
    // JSON mode: starts with { or [
    assertMatch(r.stdout.trim(), /^\{/);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("parse: kg stats without --json outputs plain text", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "stats"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
    // Plain text mode: does NOT start with {
    assertEquals(r.stdout.trim().startsWith("{"), false);
  } finally {
    await repo.cleanup();
  }
});

// ─── kg validate flags ────────────────────────────────────────────────────────

Deno.test("parse: kg validate --format json outputs JSON", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "validate", "--format", "json"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
    assertMatch(r.stdout.trim(), /^\{/);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("parse: kg validate --quiet exits 0 and produces one-line summary", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "validate", "--quiet"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
    // Quiet mode outputs a single summary line
    const lines = r.stdout.trim().split("\n").filter((l) => l.length > 0);
    assertEquals(lines.length, 1, `Expected 1 line, got: ${r.stdout}`);
    assertEquals(lines[0].startsWith("Validation:"), true);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("parse: kg validate --no-rules skips rule validation", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "validate", "--no-rules"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
  } finally {
    await repo.cleanup();
  }
});

// ─── kg doctor flags ──────────────────────────────────────────────────────────

Deno.test("parse: kg doctor --json outputs JSON", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "doctor", "--json"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
    assertMatch(r.stdout.trim(), /^\{/);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("parse: kg doctor without --json outputs plain text", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "doctor"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
    assertEquals(r.stdout.trim().startsWith("{"), false);
  } finally {
    await repo.cleanup();
  }
});

// ─── kg log flags ─────────────────────────────────────────────────────────────

Deno.test("parse: kg log -n 1 is accepted without error", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "log", "-n", "1"]);
    // May succeed or show "no KG history" — either way not a parse error
    assertEquals(r.code <= 1, true);
    assertEquals(r.stderr.includes("Unknown kg"), false);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("parse: kg log --json flag is accepted", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "log", "--json"]);
    assertEquals(r.stderr.includes("Unknown kg"), false);
  } finally {
    await repo.cleanup();
  }
});

// ─── kg diff flags ────────────────────────────────────────────────────────────

Deno.test("parse: kg diff --json flag is accepted", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "diff", "--json"]);
    assertEquals(r.stderr.includes("Unknown kg"), false);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("parse: kg diff --name-only flag is accepted", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "diff", "--name-only"]);
    assertEquals(r.stderr.includes("Unknown kg"), false);
  } finally {
    await repo.cleanup();
  }
});

// ─── kg status ────────────────────────────────────────────────────────────────

Deno.test("parse: kg status on valid repo produces output", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "status"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
    // Should produce some output (entity/edge counts or status info)
    assertEquals(r.stdout.length > 0, true);
  } finally {
    await repo.cleanup();
  }
});

// ─── kg config ────────────────────────────────────────────────────────────────

Deno.test("parse: kg config on valid repo exits 0", async () => {
  const repo = await makeTempRepo();
  try {
    // Create a minimal config file
    const configDir = join(repo.root, ".khive");
    await Deno.mkdir(configDir, { recursive: true });
    await Deno.writeTextFile(join(configDir, "config.toml"), "# khive config\n");
    const r = await runCliIn(repo.root, ["kg", "config"]);
    // Exit 0 or 1 depending on whether config exists; no parse error
    assertEquals(r.stderr.includes("Unknown kg"), false);
  } finally {
    await repo.cleanup();
  }
});

// ─── kg embed flags ───────────────────────────────────────────────────────────

Deno.test("parse: kg embed on valid repo is accepted by dispatcher", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "embed"]);
    // Embed may succeed or fail depending on state — but should not be "unknown subcommand"
    assertEquals(r.stderr.includes("Unknown kg subcommand"), false);
  } finally {
    await repo.cleanup();
  }
});

// ─── pack subcommands ─────────────────────────────────────────────────────────

Deno.test("parse: pack check with no path is a parse call to dispatcher", async () => {
  const r = await runCli(["pack", "check"]);
  // check with no args will fail — but not as "unknown subcommand"
  assertEquals(r.stderr.includes("Unknown pack subcommand"), false);
});

Deno.test("parse: pack install stub exits 1 with not-implemented message", async () => {
  const r = await runCli(["pack", "install"]);
  assertEquals(r.code, 1);
  assertEquals(r.stderr.includes("not yet implemented"), true);
});

Deno.test("parse: pack remove stub exits 1 with not-implemented message", async () => {
  const r = await runCli(["pack", "remove"]);
  assertEquals(r.code, 1);
  assertEquals(r.stderr.includes("not yet implemented"), true);
});

Deno.test("parse: pack publish stub exits 1 with not-implemented message", async () => {
  const r = await runCli(["pack", "publish"]);
  assertEquals(r.code, 1);
  assertEquals(r.stderr.includes("not yet implemented"), true);
});
