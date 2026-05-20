/**
 * Tests for git utility functions.
 */

import { assertEquals, assertRejects } from "@std/assert";
import { getCurrentBranch, getRepoRoot, isGitRepo } from "./git.ts";

async function makeTempRepo(): Promise<string> {
  const dir = await Deno.makeTempDir({ prefix: "khive_git_test_" });
  const init = new Deno.Command("git", {
    args: ["init", dir],
    stdout: "piped",
    stderr: "piped",
  });
  const out = await init.output();
  if (out.code !== 0) {
    throw new Error(`git init failed: ${new TextDecoder().decode(out.stderr)}`);
  }
  // Minimal identity for commits.
  for (
    const [k, v] of [
      ["user.email", "test@example.com"],
      ["user.name", "Test"],
    ]
  ) {
    const cfg = new Deno.Command("git", {
      args: ["-C", dir, "config", k, v],
      stdout: "piped",
      stderr: "piped",
    });
    await cfg.output();
  }
  return dir;
}

async function removeDir(path: string): Promise<void> {
  await Deno.remove(path, { recursive: true });
}

Deno.test("isGitRepo: returns true inside a git repo", async () => {
  const dir = await makeTempRepo();
  try {
    const orig = Deno.cwd();
    Deno.chdir(dir);
    try {
      assertEquals(await isGitRepo(), true);
    } finally {
      Deno.chdir(orig);
    }
  } finally {
    await removeDir(dir);
  }
});

Deno.test("isGitRepo: returns false outside a git repo", async () => {
  const dir = await Deno.makeTempDir({ prefix: "khive_nogit_" });
  try {
    const orig = Deno.cwd();
    Deno.chdir(dir);
    try {
      assertEquals(await isGitRepo(), false);
    } finally {
      Deno.chdir(orig);
    }
  } finally {
    await removeDir(dir);
  }
});

Deno.test("getRepoRoot: returns the repo root", async () => {
  const dir = await makeTempRepo();
  try {
    const orig = Deno.cwd();
    Deno.chdir(dir);
    try {
      const root = await getRepoRoot();
      // On macOS, /var is a symlink to /private/var, so we compare just the
      // directory name rather than the full path.
      assertEquals(root.endsWith(dir.split("/").at(-1)!), true);
    } finally {
      Deno.chdir(orig);
    }
  } finally {
    await removeDir(dir);
  }
});

Deno.test("getRepoRoot: throws outside a git repo", async () => {
  const dir = await Deno.makeTempDir({ prefix: "khive_nogit_" });
  try {
    const orig = Deno.cwd();
    Deno.chdir(dir);
    try {
      await assertRejects(() => getRepoRoot(), Error, "Not a git repository");
    } finally {
      Deno.chdir(orig);
    }
  } finally {
    await removeDir(dir);
  }
});

Deno.test("getCurrentBranch: returns a branch name inside a git repo", async () => {
  const dir = await makeTempRepo();
  try {
    // Create an initial commit so HEAD points to a branch.
    await Deno.writeTextFile(`${dir}/README.md`, "test");
    const add = new Deno.Command("git", {
      args: ["-C", dir, "add", "README.md"],
      stdout: "piped",
      stderr: "piped",
    });
    await add.output();
    const commit = new Deno.Command("git", {
      args: ["-C", dir, "commit", "-m", "init"],
      stdout: "piped",
      stderr: "piped",
    });
    await commit.output();

    const orig = Deno.cwd();
    Deno.chdir(dir);
    try {
      const branch = await getCurrentBranch();
      // Should be "main" or "master" depending on git config.
      assertEquals(branch === "main" || branch === "master", true);
    } finally {
      Deno.chdir(orig);
    }
  } finally {
    await removeDir(dir);
  }
});
