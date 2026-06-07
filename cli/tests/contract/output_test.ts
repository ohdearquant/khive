/**
 * Contract tests: structured output shape validation.
 */

import { assertEquals, assertMatch } from "@std/assert";
import { assertJsonShape, makeTempRepo, runCli, runCliIn } from "../helpers.ts";

// ─── Version output ────────────────────────────────────────────────────────────

Deno.test("output: khive --version prints version string", async () => {
  const r = await runCli(["--version"]);
  assertEquals(r.code, 0);
  // Must contain a semver-like version
  assertMatch(r.stdout.trim(), /\d+\.\d+\.\d+/);
});

Deno.test("output: khive -V prints version string", async () => {
  const r = await runCli(["-V"]);
  assertEquals(r.code, 0);
  assertMatch(r.stdout.trim(), /\d+\.\d+\.\d+/);
});

Deno.test("output: khive --version output starts with 'khive '", async () => {
  const r = await runCli(["--version"]);
  assertEquals(r.code, 0);
  assertEquals(r.stdout.trim().startsWith("khive "), true);
});

// ─── stats --json output shape ─────────────────────────────────────────────────

Deno.test("output: kg stats --json emits valid JSON with expected keys", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "stats", "--json"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
    assertJsonShape(r.stdout, ["entityCount", "edgeCount"]);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("output: kg stats --json entityCount is a number", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "stats", "--json"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
    const data = JSON.parse(r.stdout) as Record<string, unknown>;
    assertEquals(typeof data.entityCount, "number");
  } finally {
    await repo.cleanup();
  }
});

// ─── validate --format json output shape ──────────────────────────────────────

Deno.test("output: kg validate --format json emits valid JSON with summary", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "validate", "--format", "json"]);
    // Exit 0 on valid KG (warnings only do not cause failure)
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
    // JSON shape: { rules: [...], summary: { passed, errors, warnings, ... } }
    assertJsonShape(r.stdout, ["rules", "summary"]);
  } finally {
    await repo.cleanup();
  }
});

Deno.test("output: kg validate --format json summary.passed is true for clean KG", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "validate", "--format", "json"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
    const data = JSON.parse(r.stdout) as { summary: { passed: boolean } };
    assertEquals(data.summary.passed, true);
  } finally {
    await repo.cleanup();
  }
});

// ─── doctor --json output shape ────────────────────────────────────────────────

Deno.test("output: kg doctor --json emits valid JSON", async () => {
  const repo = await makeTempRepo();
  try {
    const r = await runCliIn(repo.root, ["kg", "doctor", "--json"]);
    assertEquals(r.code, 0, `stderr: ${r.stderr}`);
    assertJsonShape(r.stdout, ["valid"]);
  } finally {
    await repo.cleanup();
  }
});
