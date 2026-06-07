/**
 * Thin wrappers around the git CLI (ADR-048 §4, ADR-051 §3).
 * All operations use Deno.Command — no deprecated Deno.run.
 */

export interface ExecResult {
  code: number;
  stdout: string;
  stderr: string;
}

/**
 * Execute a command and return its exit code, stdout, and stderr.
 * Does not throw on non-zero exit — callers decide how to handle failures.
 */
export async function exec(cmd: string[]): Promise<ExecResult> {
  const command = new Deno.Command(cmd[0], {
    args: cmd.slice(1),
    stdout: "piped",
    stderr: "piped",
  });
  const output = await command.output();
  return {
    code: output.code,
    stdout: new TextDecoder().decode(output.stdout).trim(),
    stderr: new TextDecoder().decode(output.stderr).trim(),
  };
}

/**
 * Get the absolute path of the git repository root.
 * Throws if the current directory is not inside a git repository.
 */
export async function getRepoRoot(): Promise<string> {
  const result = await exec(["git", "rev-parse", "--show-toplevel"]);
  if (result.code !== 0) {
    throw new Error(
      `Not a git repository. Run 'git init' first.\n${result.stderr}`,
    );
  }
  return result.stdout;
}

/** Returns true if the current directory is inside a git repository. */
export async function isGitRepo(): Promise<boolean> {
  const result = await exec(["git", "rev-parse", "--show-toplevel"]);
  return result.code === 0;
}

/** Stage the given files with git add. */
export async function gitAdd(files: string[]): Promise<void> {
  if (files.length === 0) return;
  const result = await exec(["git", "add", ...files]);
  if (result.code !== 0) {
    throw new Error(`git add failed: ${result.stderr}`);
  }
}

/**
 * Create a git commit with the given message.
 * Returns the short commit SHA (7 chars) on success.
 */
export async function gitCommit(message: string): Promise<string> {
  const result = await exec(["git", "commit", "-m", message]);
  if (result.code !== 0) {
    throw new Error(`git commit failed: ${result.stderr}`);
  }
  // Extract short SHA from output like "[main a1b2c3d] message"
  const match = result.stdout.match(/\[.*? ([0-9a-f]{7,})\]/);
  return match ? match[1] : "";
}

/**
 * Return the name of the current git branch, or a detached HEAD description.
 * Uses `symbolic-ref --short HEAD` so unborn branches in fresh repos still work.
 */
export async function getCurrentBranch(): Promise<string> {
  const result = await exec(["git", "symbolic-ref", "--short", "HEAD"]);
  if (result.code === 0) {
    return result.stdout;
  }

  const head = await exec(["git", "rev-parse", "--short", "HEAD"]);
  if (head.code === 0 && head.stdout) {
    return `HEAD detached at ${head.stdout}`;
  }

  throw new Error(`Failed to get current branch: ${result.stderr || head.stderr}`);
}

/** Return the path to the .git directory for the current repo. */
export async function getGitDir(repoRoot: string): Promise<string> {
  const result = await exec(["git", "rev-parse", "--git-dir"]);
  if (result.code !== 0) {
    throw new Error(`Failed to locate .git directory: ${result.stderr}`);
  }
  const raw = result.stdout;
  // Absolute if it starts with /, otherwise relative to repoRoot
  if (raw.startsWith("/")) return raw;
  return `${repoRoot}/${raw}`;
}
