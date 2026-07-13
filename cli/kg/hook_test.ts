/**
 * Integration tests for `khive kg hook` — ADR-056 §3 symlink model.
 *
 * Covers a finding from review:
 *   - install writes tracked script + symlink (not direct file)
 *   - status reports symlink validity
 *   - uninstall removes symlink but preserves tracked script
 *   - hook content uses 'khive kg validate' without --strict
 *   - hook content fails closed when khive is unavailable
 *
 * Tests call installHook/uninstallHook/statusHook directly with explicit
 * (repoRoot, gitDir) so they operate on an isolated temp directory and
 * never touch the actual worktree's .git.
 */

import { assertEquals, assertStringIncludes } from "@std/assert";
import { join } from "@std/path";
import { installHook, statusHook, uninstallHook } from "./hook.ts";

// Create a minimal isolated git-like fixture (no real git binary needed —
// just the directory structure that the hook functions expect).
async function withGitRepo(fn: (root: string, gitDir: string) => Promise<void>): Promise<void> {
  const root = await Deno.makeTempDir({ prefix: "khive-hook-test-" });
  const gitDir = join(root, ".git");
  await Deno.mkdir(join(gitDir, "hooks"), { recursive: true });
  await Deno.writeTextFile(join(gitDir, "HEAD"), "ref: refs/heads/main\n");
  await Deno.mkdir(join(root, ".khive/kg"), { recursive: true });
  try {
    await fn(root, gitDir);
  } finally {
    await Deno.remove(root, { recursive: true });
  }
}

// ─── install creates symlink model ───────────────────────────────────────────

Deno.test("hook install — creates tracked script at .khive/kg/hooks/pre-commit", async () => {
  await withGitRepo(async (root, gitDir) => {
    const code = await installHook(root, gitDir);
    assertEquals(code, 0);

    const trackedPath = join(root, ".khive/kg/hooks/pre-commit");
    const stat = await Deno.stat(trackedPath);
    assertEquals(stat.isFile, true, "tracked script must exist as a regular file");
  });
});

Deno.test("hook install — .git/hooks/pre-commit is a symlink", async () => {
  await withGitRepo(async (root, gitDir) => {
    const code = await installHook(root, gitDir);
    assertEquals(code, 0);

    const gitHookPath = join(gitDir, "hooks/pre-commit");
    const lstat = await Deno.lstat(gitHookPath);
    assertEquals(lstat.isSymlink, true, ".git/hooks/pre-commit must be a symlink");
  });
});

Deno.test("hook install — symlink resolves to tracked script", async () => {
  await withGitRepo(async (root, gitDir) => {
    await installHook(root, gitDir);

    const gitHookPath = join(gitDir, "hooks/pre-commit");
    const trackedPath = join(root, ".khive/kg/hooks/pre-commit");

    const target = await Deno.readLink(gitHookPath);
    const resolved = target.startsWith("/") ? target : join(gitDir, "hooks", target);
    const canonicalTracked = await Deno.realPath(trackedPath);
    assertEquals(
      await Deno.realPath(resolved).catch(() => resolved),
      canonicalTracked,
      "symlink must resolve to the tracked script",
    );
  });
});

// ─── hook script content ─────────────────────────────────────────────────────

Deno.test("hook script — does NOT contain --strict (only errors block, not warnings)", async () => {
  await withGitRepo(async (root, gitDir) => {
    await installHook(root, gitDir);
    const trackedPath = join(root, ".khive/kg/hooks/pre-commit");
    const content = await Deno.readTextFile(trackedPath);
    assertEquals(content.includes("--strict"), false, "hook must not use --strict");
  });
});

Deno.test("hook script — fails closed when khive is not on PATH", async () => {
  await withGitRepo(async (root, gitDir) => {
    await installHook(root, gitDir);
    const trackedPath = join(root, ".khive/kg/hooks/pre-commit");
    const content = await Deno.readTextFile(trackedPath);
    // The hook must exit non-zero (exit 1) if khive is missing AND KG files are staged.
    assertStringIncludes(content, "exit 1", "hook must fail closed (exit 1) when khive missing");
    // Must NOT silently skip by exiting 0 when khive is not found.
    // The pattern "skipping" followed by "exit 0" would be the old silent-skip behaviour.
    const silentSkip = /skipping[^\n]*\n[^\n]*exit 0/.test(content) ||
      (content.includes("skipping") && content.includes("exit 0"));
    assertEquals(silentSkip, false, "hook must NOT silently skip when khive missing");
  });
});

Deno.test("hook script — contains 'khive kg validate'", async () => {
  await withGitRepo(async (root, gitDir) => {
    await installHook(root, gitDir);
    const trackedPath = join(root, ".khive/kg/hooks/pre-commit");
    const content = await Deno.readTextFile(trackedPath);
    assertStringIncludes(content, "khive kg validate");
  });
});

// ─── status reports correctly ─────────────────────────────────────────────────

Deno.test("hook status — returns 0 when no hook installed", async () => {
  await withGitRepo(async (root, gitDir) => {
    const code = await statusHook(root, gitDir);
    assertEquals(code, 0);
  });
});

Deno.test("hook status — returns 0 after install", async () => {
  await withGitRepo(async (root, gitDir) => {
    await installHook(root, gitDir);
    const code = await statusHook(root, gitDir);
    assertEquals(code, 0);
  });
});

// ─── uninstall removes symlink but keeps tracked script ──────────────────────

Deno.test("hook uninstall — removes .git/hooks/pre-commit but keeps tracked script", async () => {
  await withGitRepo(async (root, gitDir) => {
    await installHook(root, gitDir);
    const code = await uninstallHook(root, gitDir);
    assertEquals(code, 0);

    // .git/hooks/pre-commit must be gone.
    const gitHookPath = join(gitDir, "hooks/pre-commit");
    let gitHookExists = false;
    try {
      await Deno.lstat(gitHookPath);
      gitHookExists = true;
    } catch {
      // expected
    }
    assertEquals(gitHookExists, false, ".git/hooks/pre-commit must be removed");

    // Tracked script must still exist.
    const trackedPath = join(root, ".khive/kg/hooks/pre-commit");
    const stat = await Deno.stat(trackedPath);
    assertEquals(stat.isFile, true, "tracked script must be preserved after uninstall");
  });
});

// ─── install is idempotent ────────────────────────────────────────────────────

Deno.test("hook install — second install is idempotent (returns 0)", async () => {
  await withGitRepo(async (root, gitDir) => {
    const code1 = await installHook(root, gitDir);
    assertEquals(code1, 0);
    const code2 = await installHook(root, gitDir);
    assertEquals(code2, 0);
  });
});

// ─── uninstall on non-khive hook returns error ────────────────────────────────

Deno.test("hook uninstall — returns 1 for non-khive hook", async () => {
  await withGitRepo(async (root, gitDir) => {
    // Write a foreign hook.
    await Deno.writeTextFile(
      join(gitDir, "hooks/pre-commit"),
      "#!/bin/sh\necho 'my hook'\n",
    );
    const code = await uninstallHook(root, gitDir);
    assertEquals(code, 1, "uninstall must return 1 for non-khive hook");
  });
});
