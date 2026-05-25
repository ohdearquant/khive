/**
 * Shared test utilities for khive CLI subprocess tests.
 */

import { join } from "@std/path";
import { assertEquals, assertMatch } from "@std/assert";

const CLI_ENTRY = new URL("../main.ts", import.meta.url).pathname;

export interface CliResult {
  code: number;
  stdout: string;
  stderr: string;
}

/**
 * Run the CLI with the given args as a subprocess.
 * Always resolves (never throws) — check code/stderr for failures.
 */
export async function runCli(args: string[]): Promise<CliResult> {
  const cmd = new Deno.Command(Deno.execPath(), {
    args: ["run", "--allow-all", CLI_ENTRY, ...args],
    stdout: "piped",
    stderr: "piped",
    env: { ...Deno.env.toObject(), NO_COLOR: "1" },
  });
  const { code, stdout, stderr } = await cmd.output();
  return {
    code,
    stdout: new TextDecoder().decode(stdout),
    stderr: new TextDecoder().decode(stderr),
  };
}

/**
 * Compare actual output against a golden file.
 * If UPDATE_GOLDEN=1, write the golden file instead of comparing.
 */
export function assertGolden(actual: string, goldenPath: string): void {
  if (Deno.env.get("UPDATE_GOLDEN") === "1") {
    Deno.writeTextFileSync(goldenPath, actual);
    return;
  }
  const expected = Deno.readTextFileSync(goldenPath);
  assertEquals(actual.trim(), expected.trim());
}

/**
 * Parse JSON and assert all required keys are present.
 */
export function assertJsonShape(json: string, requiredKeys: string[]): void {
  let parsed: Record<string, unknown>;
  try {
    parsed = JSON.parse(json);
  } catch {
    throw new Error(`Output is not valid JSON:\n${json}`);
  }
  for (const key of requiredKeys) {
    if (!(key in parsed)) {
      throw new Error(`Missing required key '${key}' in JSON output:\n${json}`);
    }
  }
}

/**
 * Assert that the output matches a semver pattern.
 */
export function assertSemver(version: string): void {
  assertMatch(version.trim(), /^\d+\.\d+\.\d+/);
}

// ─── Temp repo helpers ────────────────────────────────────────────────────────

export interface TempRepo {
  root: string;
  cleanup: () => Promise<void>;
}

/** Minimal .khive/kg/ structure for tests that need a valid KG directory. */
const MINIMAL_ENTITIES =
  `{"id":"00000000-0000-0000-0000-000000000001","name":"Test Entity","kind":"concept"}\n`;
const MINIMAL_EDGES = "";
// format_version and all 6 entity kinds required by validate.ts
const MINIMAL_SCHEMA = `format_version: "1.0.0"
ontology_version: "1.0.0"
entity_kinds:
  - concept
  - document
  - dataset
  - project
  - person
  - org
edge_relations:
  - relation: contains
    category: structure
  - relation: part_of
    category: structure
`;

/**
 * Create a temp directory with a minimal git repo + .khive/kg/ structure.
 */
export async function makeTempRepo(): Promise<TempRepo> {
  const root = await Deno.makeTempDir({ prefix: "khive_test_" });

  // Init git repo
  await new Deno.Command("git", {
    args: ["init", root],
    stdout: "null",
    stderr: "null",
  }).output();

  await new Deno.Command("git", {
    args: ["-C", root, "config", "user.email", "test@test.com"],
    stdout: "null",
    stderr: "null",
  }).output();

  await new Deno.Command("git", {
    args: ["-C", root, "config", "user.name", "Test"],
    stdout: "null",
    stderr: "null",
  }).output();

  // Create .khive/kg/ structure
  const kgDir = join(root, ".khive", "kg");
  await Deno.mkdir(kgDir, { recursive: true });
  await Deno.writeTextFile(join(kgDir, "entities.ndjson"), MINIMAL_ENTITIES);
  await Deno.writeTextFile(join(kgDir, "edges.ndjson"), MINIMAL_EDGES);
  await Deno.writeTextFile(join(kgDir, "schema.yaml"), MINIMAL_SCHEMA);

  // Stage files
  await new Deno.Command("git", {
    args: ["-C", root, "add", "-A"],
    stdout: "null",
    stderr: "null",
  }).output();

  await new Deno.Command("git", {
    args: ["-C", root, "commit", "-m", "init", "--no-gpg-sign"],
    stdout: "null",
    stderr: "null",
  }).output();

  return {
    root,
    cleanup: () => Deno.remove(root, { recursive: true }),
  };
}

/**
 * Run CLI from within a specific working directory.
 */
export async function runCliIn(cwd: string, args: string[]): Promise<CliResult> {
  const cmd = new Deno.Command(Deno.execPath(), {
    args: ["run", "--allow-all", CLI_ENTRY, ...args],
    cwd,
    stdout: "piped",
    stderr: "piped",
    env: { ...Deno.env.toObject(), NO_COLOR: "1" },
  });
  const { code, stdout, stderr } = await cmd.output();
  return {
    code,
    stdout: new TextDecoder().decode(stdout),
    stderr: new TextDecoder().decode(stderr),
  };
}
