/**
 * khive kg init — initialise a git-native KG in the current repository.
 *
 * Per ADR-048 §4 + ADR-051 §6 + ADR-057 §4:
 *   1. Verify we are inside a git repo.
 *   2. Create .khive/kg/ with entities.ndjson, edges.ndjson, schema.yaml,
 *      and migrations/.gitkeep.
 *   3. Create .khive/config.toml with built-in defaults (ADR-057 §4).
 *   4. Create .khive/state/ directory; write HEAD (current branch name).
 *   5. Install post-checkout, post-merge, post-rewrite hooks (ADR-051 §6).
 *   6. Stage created files with git add.
 *   7. Print a summary.
 */

import { join } from "@std/path";
import { exec, getCurrentBranch, getGitDir, getRepoRoot, gitAdd, isGitRepo } from "../lib/git.ts";
import { DEFAULT_SCHEMA_YAML } from "../lib/schema.ts";
import {
  CONFIG_FILE,
  EDGES_FILE,
  ENTITIES_FILE,
  KG_DIR,
  MIGRATIONS_DIR,
  REMOTE_CACHE_DIR,
  SCHEMA_FILE,
  STATE_DIR,
} from "../lib/paths.ts";

// ---------------------------------------------------------------------------
// Project config template (ADR-057 §4)
// ---------------------------------------------------------------------------

const DEFAULT_CONFIG_TOML = `\
# .khive/config.toml — project KG configuration
# Committed to git. All collaborators use these settings.
# See: https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-035-cli-config-and-auto-embed.md

[embed]
model = "mE5-small"
dimensions = 384
auto_embed = true
batch_size = 64

[embed.fields]
include = ["name", "description"]

[schema]
strict = true
`;

// ---------------------------------------------------------------------------
// .khive/.gitignore — allowlist pattern (ignore everything except KG data)
// ---------------------------------------------------------------------------

const KHIVE_GITIGNORE = `\
# Ignore everything inside .khive/ by default.
# Only the KG data and project config are committed to git.
*

# KG data files (committed)
!.gitignore
!kg/
!kg/**
!config.toml

# Remote cache and derived working state are never committed.
kg/.remote-cache/
kg/.remote-cache/**
`;

// ---------------------------------------------------------------------------
// Git hook content (ADR-051 §6)
// ---------------------------------------------------------------------------

const HOOK_NAMES = ["post-checkout", "post-merge", "post-rewrite"] as const;
type HookName = typeof HOOK_NAMES[number];

function hookContent(name: HookName): string {
  const checkoutGuard = name === "post-checkout"
    ? `
# Git passes old-ref, new-ref, and checkout type. For branch checkouts,
# skip the DB sync when KG files are identical between the two refs.
if [ "\${3:-}" = "1" ] && [ -n "\${1:-}" ] && [ -n "\${2:-}" ]; then
  if git diff --quiet "$1" "$2" -- .khive/kg; then
    exit 0
  fi
fi
`
    : "";

  return `#!/bin/sh
# Installed by khive kg init. Rebuilds working.db after git operations.
# Add your own logic below this line — do not remove the khive line.
${checkoutGuard}if command -v khive >/dev/null 2>&1; then
  khive kg sync --quiet 2>/dev/null || true
fi
`;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Write a file only if it does not already exist. Returns true if written. */
async function writeIfAbsent(
  path: string,
  content: string,
  mode = 0o644,
): Promise<boolean> {
  try {
    await Deno.stat(path);
    return false; // already exists
  } catch {
    await Deno.writeTextFile(path, content);
    await Deno.chmod(path, mode);
    return true;
  }
}

// ---------------------------------------------------------------------------
// Hook installation (ADR-051 §6)
// ---------------------------------------------------------------------------

interface HookResult {
  name: string;
  installed: boolean;
  existed: boolean;
}

async function installHooks(gitDir: string): Promise<HookResult[]> {
  const hooksDir = join(gitDir, "hooks");
  try {
    await Deno.mkdir(hooksDir, { recursive: true });
  } catch {
    // hooks dir already exists
  }

  const results: HookResult[] = [];
  for (const name of HOOK_NAMES) {
    const hookPath = join(hooksDir, name);
    try {
      await Deno.stat(hookPath);
      // Hook exists — do not overwrite.
      results.push({ name, installed: false, existed: true });
    } catch {
      // Does not exist — create it.
      await Deno.writeTextFile(hookPath, hookContent(name));
      await Deno.chmod(hookPath, 0o755);
      results.push({ name, installed: true, existed: false });
    }
  }
  return results;
}

// ---------------------------------------------------------------------------
// Main command
// ---------------------------------------------------------------------------

export async function kgInit(): Promise<void> {
  // 1. Ensure we are in a git repo. Per ADR-052 §9 step 2: if the current
  //    directory is not a git repository, run `git init` automatically.
  if (!(await isGitRepo())) {
    console.log("Not a git repository — running 'git init'...");
    const result = await exec(["git", "init"]);
    if (result.code !== 0) {
      console.error(`Error: git init failed: ${result.stderr}`);
      Deno.exit(1);
    }
    console.log(result.stdout || "Initialized empty Git repository.");
  }

  let repoRoot: string;
  try {
    repoRoot = await getRepoRoot();
  } catch (err) {
    console.error(`Error: Failed to locate git repository root: ${(err as Error).message}`);
    Deno.exit(1);
  }

  // 2. Guard: refuse to init if .khive/kg/ already exists.
  const kgDirPath = join(repoRoot, KG_DIR);
  try {
    await Deno.stat(kgDirPath);
    console.error(
      `Error: ${KG_DIR} already exists. The KG is already initialised.`,
    );
    Deno.exit(1);
  } catch {
    // Not found — good.
  }

  const created: string[] = [];

  // 3. Create .khive/kg/ structure.
  await Deno.mkdir(kgDirPath, { recursive: true });
  await Deno.mkdir(join(repoRoot, MIGRATIONS_DIR), { recursive: true });
  // .remote-cache is gitignored (ADR-048 §Implementation).
  await Deno.mkdir(join(repoRoot, REMOTE_CACHE_DIR), { recursive: true });

  await Deno.writeTextFile(join(repoRoot, ENTITIES_FILE), "");
  created.push(ENTITIES_FILE);

  await Deno.writeTextFile(join(repoRoot, EDGES_FILE), "");
  created.push(EDGES_FILE);

  await Deno.writeTextFile(join(repoRoot, SCHEMA_FILE), DEFAULT_SCHEMA_YAML);
  await Deno.chmod(join(repoRoot, SCHEMA_FILE), 0o644);
  created.push(SCHEMA_FILE);

  // migrations/.gitkeep ensures the directory is tracked in git.
  const gitkeepPath = join(repoRoot, MIGRATIONS_DIR, ".gitkeep");
  await Deno.writeTextFile(gitkeepPath, "");
  created.push(`${MIGRATIONS_DIR}/.gitkeep`);

  // 4. Create .khive/config.toml (if absent).
  const configPath = join(repoRoot, CONFIG_FILE);
  const configWritten = await writeIfAbsent(configPath, DEFAULT_CONFIG_TOML, 0o644);
  if (configWritten) {
    created.push(CONFIG_FILE);
  } else {
    console.log(`Note: ${CONFIG_FILE} already exists — not overwritten.`);
  }

  // 5. Create .khive/state/ (gitignored via .khive/.gitignore allowlist).
  const stateDirPath = join(repoRoot, STATE_DIR);
  try {
    await Deno.mkdir(stateDirPath, { recursive: true });
  } catch {
    // Already exists.
  }

  // Write current branch name to .khive/state/HEAD (ADR-052 §9).
  const branch = await getCurrentBranch();
  await Deno.writeTextFile(join(stateDirPath, "HEAD"), branch + "\n");

  // Write .khive/.gitignore allowlist (ignore everything except KG data).
  const khiveGitignorePath = join(repoRoot, ".khive", ".gitignore");
  const khiveGitignoreWritten = await writeIfAbsent(
    khiveGitignorePath,
    KHIVE_GITIGNORE,
    0o644,
  );
  if (khiveGitignoreWritten) {
    created.push(".khive/.gitignore");
  }

  // 6. Install git hooks.
  const gitDir = await getGitDir(repoRoot);
  const hookResults = await installHooks(gitDir);

  // 7. Stage created files.
  await gitAdd([
    join(repoRoot, KG_DIR),
    join(repoRoot, CONFIG_FILE),
    join(repoRoot, ".khive", ".gitignore"),
  ]);

  // 8. Print summary.
  console.log(`Initialised KG in ${repoRoot}`);
  console.log("");
  console.log("Created:");
  for (const f of created) {
    console.log(`  ${f}`);
  }
  console.log("");

  console.log("Git hooks:");
  for (const h of hookResults) {
    if (h.installed) {
      console.log(`  Installed ${h.name}`);
    } else {
      console.log(
        `  Skipped ${h.name} (already exists) — add 'khive kg sync --quiet' manually.`,
      );
    }
  }

  console.log("");
  console.log("Next steps:");
  console.log("  git commit -m 'chore: initialise KG'");
  console.log("  khive kg export       # export DB → NDJSON after first entities");
  console.log("  khive kg commit -m 'feat: add initial entities'");
}
