/**
 * `khive kg hook` — manage the pre-commit validation hook (ADR-056 §3).
 *
 * Subcommands:
 *   khive kg hook install     — write .khive/kg/hooks/pre-commit, install
 *                               .git/hooks/pre-commit symlink pointing to it
 *   khive kg hook uninstall   — remove the symlink; leave tracked script intact
 *   khive kg hook status      — print install state (symlink valid / broken / absent)
 *
 * The hook is stored at `.khive/kg/hooks/pre-commit` so it is tracked by git
 * alongside the KG and its rules. The `.git/hooks/pre-commit` entry is a
 * symlink to the tracked script (ADR-056 §3).
 *
 * Hook behaviour (ADR-056 §3):
 *   - Runs only when entities.ndjson or edges.ndjson are staged.
 *   - Exits non-zero (blocks commit) only on severity `error` violations.
 *   - Warnings and info are printed but do NOT block the commit.
 *   - If `khive` is not on PATH and KG files are staged, fails closed with an
 *     actionable error (rather than silently exiting 0).
 *   - `git commit --no-verify` bypasses the hook per git conventions.
 */

import { join, relative } from "@std/path";
import { getGitDir, getRepoRoot } from "../lib/git.ts";

const HOOK_NAME = "pre-commit";

/**
 * The tracked hook script written to `.khive/kg/hooks/pre-commit`.
 *
 * Uses `khive kg validate` (no --strict) so only `error`-severity violations
 * block commits. Warnings and info are printed but allow the commit to proceed
 * (ADR-056 §3: "Exits non-zero if any rule at severity `error` is violated").
 *
 * Fail-closed behaviour: if `khive` is not on PATH and KG files are staged,
 * the hook prints an actionable error and exits non-zero.
 */
const HOOK_SCRIPT_CONTENT = `#!/usr/bin/env bash
# Installed by 'khive kg hook install' (ADR-056).
# Validates .khive/kg/ files against schema + rules before committing.
# Only severity=error violations block the commit; warnings are printed only.
# Bypass with: git commit --no-verify

set -euo pipefail

staged=$(git diff --cached --name-only | grep -E '^\\.khive/kg/(entities|edges)\\.ndjson$' || true)
if [ -z "$staged" ]; then
  exit 0  # No KG files staged — skip validation.
fi

if ! command -v khive >/dev/null 2>&1; then
  echo "ERROR: khive pre-commit hook: KG files are staged but 'khive' is not on PATH." >&2
  echo "  Install khive or bypass with: git commit --no-verify" >&2
  exit 1
fi

khive kg validate --quiet
`;

// Relative path from repo root for the tracked hook script.
const TRACKED_HOOK_RELPATH = ".khive/kg/hooks/pre-commit";

interface HookState {
  trackedScriptPath: string;
  trackedScriptExists: boolean;
  gitHookPath: string;
  gitHookExists: boolean;
  isSymlink: boolean;
  symlinkValid: boolean;
  installedByKhive: boolean;
}

async function probeHook(repoRoot: string, gitDir: string): Promise<HookState> {
  const trackedScriptPath = join(repoRoot, TRACKED_HOOK_RELPATH);
  const gitHookPath = join(gitDir, "hooks", HOOK_NAME);

  // Check tracked script.
  let trackedScriptExists = false;
  try {
    await Deno.stat(trackedScriptPath);
    trackedScriptExists = true;
  } catch {
    // Not found.
  }

  // Check .git/hooks/pre-commit.
  let gitHookExists = false;
  let isSymlink = false;
  let symlinkValid = false;
  let installedByKhive = false;

  try {
    const lstatInfo = await Deno.lstat(gitHookPath);
    gitHookExists = true;
    isSymlink = lstatInfo.isSymlink;

    if (isSymlink) {
      // Verify symlink target resolves to our tracked script.
      try {
        const target = await Deno.readLink(gitHookPath);
        // Resolve relative symlinks against the hook's directory.
        const resolvedTarget = target.startsWith("/") ? target : join(gitDir, "hooks", target);
        const canonicalTracked = trackedScriptPath;
        // Normalise both to absolute paths for comparison.
        symlinkValid = resolvedTarget === canonicalTracked ||
          resolvedTarget === await Deno.realPath(trackedScriptPath).catch(() => null);
        installedByKhive = symlinkValid;
      } catch {
        // Broken symlink.
        symlinkValid = false;
      }
    } else {
      // Regular file — check if it was written by a previous khive version
      // (content-based detection for migration).
      try {
        const text = await Deno.readTextFile(gitHookPath);
        installedByKhive = text.includes("khive kg validate");
      } catch {
        installedByKhive = false;
      }
    }
  } catch {
    // Does not exist.
  }

  return {
    trackedScriptPath,
    trackedScriptExists,
    gitHookPath,
    gitHookExists,
    isSymlink,
    symlinkValid,
    installedByKhive,
  };
}

export async function installHook(repoRoot: string, gitDir?: string): Promise<number> {
  const resolvedGitDir = gitDir ?? await getGitDir(repoRoot);
  const state = await probeHook(repoRoot, resolvedGitDir);

  // 1. Write the tracked script if absent or stale.
  await Deno.mkdir(join(repoRoot, ".khive/kg/hooks"), { recursive: true });
  await Deno.writeTextFile(state.trackedScriptPath, HOOK_SCRIPT_CONTENT);
  await Deno.chmod(state.trackedScriptPath, 0o755);

  // 2. Handle existing .git/hooks/pre-commit.
  if (state.gitHookExists) {
    if (state.isSymlink && state.symlinkValid) {
      console.log(`Pre-commit hook already installed at ${state.gitHookPath}`);
      console.log(`  -> ${state.trackedScriptPath}`);
      return 0;
    }
    if (state.installedByKhive) {
      // Old direct-write style — replace with symlink.
      await Deno.remove(state.gitHookPath);
    } else {
      console.error(
        `A pre-commit hook already exists at ${state.gitHookPath} ` +
          `and was not installed by khive. ` +
          `Remove or rename it manually, then re-run 'khive kg hook install'.`,
      );
      return 1;
    }
  }

  // 3. Install symlink: .git/hooks/pre-commit -> <relpath to tracked script>.
  await Deno.mkdir(join(resolvedGitDir, "hooks"), { recursive: true });
  // Use a relative path for portability (the repo can move).
  const symlinkTarget = relative(join(resolvedGitDir, "hooks"), state.trackedScriptPath);
  await Deno.symlink(symlinkTarget, state.gitHookPath);

  console.log(
    `Installed pre-commit hook: ${state.gitHookPath} -> ${TRACKED_HOOK_RELPATH}`,
  );
  console.log(
    "  Runs 'khive kg validate' (errors block; warnings printed only) before each commit.",
  );
  console.log("  Bypass with: git commit --no-verify");
  return 0;
}

export async function uninstallHook(repoRoot: string, gitDir?: string): Promise<number> {
  const resolvedGitDir = gitDir ?? await getGitDir(repoRoot);
  const state = await probeHook(repoRoot, resolvedGitDir);

  if (!state.gitHookExists) {
    console.log("No pre-commit hook installed.");
    return 0;
  }

  if (!state.installedByKhive && !(state.isSymlink && state.symlinkValid)) {
    console.error(
      `Pre-commit hook at ${state.gitHookPath} was not installed by khive. ` +
        `Leaving it in place; remove it manually if you want to delete it.`,
    );
    return 1;
  }

  // Remove only the .git/ symlink (or legacy direct file); leave tracked script.
  await Deno.remove(state.gitHookPath);
  console.log(
    `Removed symlink ${state.gitHookPath}. ` +
      `Tracked script ${TRACKED_HOOK_RELPATH} is preserved.`,
  );
  return 0;
}

export async function statusHook(repoRoot: string, gitDir?: string): Promise<number> {
  const resolvedGitDir = gitDir ?? await getGitDir(repoRoot);
  const state = await probeHook(repoRoot, resolvedGitDir);

  if (!state.gitHookExists) {
    console.log(
      `Pre-commit hook: not installed (run 'khive kg hook install' to add it).`,
    );
    return 0;
  }

  if (state.isSymlink) {
    if (state.symlinkValid) {
      const trackedStatus = state.trackedScriptExists ? "exists" : "MISSING (tracked script gone)";
      console.log(
        `Pre-commit hook: installed (symlink, khive-managed)\n` +
          `  symlink: ${state.gitHookPath}\n` +
          `  target:  ${TRACKED_HOOK_RELPATH} [${trackedStatus}]`,
      );
    } else {
      console.log(
        `Pre-commit hook: broken symlink at ${state.gitHookPath}\n` +
          `  Run 'khive kg hook install' to fix.`,
      );
    }
  } else if (state.installedByKhive) {
    console.log(
      `Pre-commit hook: installed (direct file, not symlink — legacy)\n` +
        `  ${state.gitHookPath}\n` +
        `  Run 'khive kg hook install' to migrate to symlink model.`,
    );
  } else {
    console.log(
      `Pre-commit hook: installed at ${state.gitHookPath}, but NOT by khive — leave as-is or remove manually.`,
    );
  }
  return 0;
}

function printHelp(): void {
  console.log(`Usage: khive kg hook <install|uninstall|status>

Manages the git pre-commit hook that runs 'khive kg validate' before
each commit (ADR-056 §3).

The tracked hook script is stored at .khive/kg/hooks/pre-commit (git-tracked
alongside the KG). The .git/hooks/pre-commit entry is a symlink to it.

Only severity=error violations block commits; warnings are printed but
do not prevent the commit.

Subcommands:
  install      Write .khive/kg/hooks/pre-commit and install symlink.
  uninstall    Remove the .git/ symlink; leave the tracked script.
  status       Print install state (symlink target, executable bit).`);
}

export async function runHook(
  _repoRoot: string,
  args: string[],
): Promise<number> {
  // Always resolve the actual repo root so callers can pass anything.
  let repoRoot: string;
  try {
    repoRoot = await getRepoRoot();
  } catch {
    // Fall back to passed root (e.g. in tests).
    repoRoot = _repoRoot;
  }

  const [sub] = args;
  if (!sub || sub === "--help" || sub === "-h") {
    printHelp();
    return 0;
  }
  switch (sub) {
    case "install":
      return await installHook(repoRoot, undefined);
    case "uninstall":
      return await uninstallHook(repoRoot, undefined);
    case "status":
      return await statusHook(repoRoot, undefined);
    default:
      console.error(`Unknown hook subcommand: '${sub}'`);
      printHelp();
      return 1;
  }
}
