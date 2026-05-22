/**
 * Resolve the path to the `kkernel` Rust binary (ADR-076, ADR-077).
 *
 * Strategy (in order):
 *   1. `KKERNEL_BINARY` env var — explicit override, used in dev and tests.
 *   2. `@khive/kernel-<platform>/bin/kkernel` under node_modules — production
 *      install via npm optional dependencies (ADR-077).
 *   3. `<repo>/crates/target/release/kkernel` — monorepo dev convenience.
 *   4. `<repo>/crates/target/debug/kkernel` — last-resort dev fallback.
 *
 * Throws a descriptive error when no candidate exists.
 */

import { dirname, fromFileUrl, join } from "@std/path";

function platformKey(): string {
  const os = Deno.build.os;
  const arch = Deno.build.arch;
  const map: Record<string, string> = {
    "darwin-aarch64": "darwin-arm64",
    "darwin-x86_64": "darwin-x64",
    "linux-x86_64": "linux-x64-gnu",
    "linux-aarch64": "linux-arm64",
    "windows-x86_64": "win32-x64",
  };
  const key = `${os}-${arch}`;
  return map[key] ?? key;
}

function exists(path: string): boolean {
  try {
    Deno.statSync(path);
    return true;
  } catch {
    return false;
  }
}

/**
 * Walk upward from `start` looking for a directory containing `marker`.
 * Returns the directory path or null if no ancestor matches.
 */
function findAncestor(start: string, marker: string): string | null {
  let dir = start;
  for (let i = 0; i < 16; i++) {
    if (exists(join(dir, marker))) return dir;
    const parent = dirname(dir);
    if (parent === dir) return null;
    dir = parent;
  }
  return null;
}

/**
 * Locate the kkernel binary, returning an absolute path.
 *
 * `repoRoot` is the khive repo root passed by the caller (used for
 * monorepo dev fallbacks). It does not affect production resolution.
 */
export function kkernelPath(repoRoot?: string): string {
  // 1. Explicit override via env var.
  const override = Deno.env.get("KKERNEL_BINARY");
  if (override && exists(override)) return override;

  const isWindows = Deno.build.os === "windows";
  const exe = isWindows ? "kkernel.exe" : "kkernel";

  // 2. npm optional-deps subpackage. Resolve relative to this module's path.
  const here = dirname(fromFileUrl(import.meta.url));
  const nodeModulesRoot = findAncestor(here, "node_modules");
  if (nodeModulesRoot) {
    const candidate = join(
      nodeModulesRoot,
      "node_modules",
      "@khive",
      `kernel-${platformKey()}`,
      "bin",
      exe,
    );
    if (exists(candidate)) return candidate;
  }

  // 3. Monorepo dev: <repo>/crates/target/{release,debug}/kkernel
  const candidates: string[] = [];
  if (repoRoot) {
    candidates.push(join(repoRoot, "crates", "target", "release", exe));
    candidates.push(join(repoRoot, "crates", "target", "debug", exe));
  }
  // Also try from this file's location upward to find a "crates" dir.
  const cratesRoot = findAncestor(here, "crates");
  if (cratesRoot) {
    candidates.push(join(cratesRoot, "crates", "target", "release", exe));
    candidates.push(join(cratesRoot, "crates", "target", "debug", exe));
  }
  for (const c of candidates) {
    if (exists(c)) return c;
  }

  throw new Error(
    `kkernel binary not found.\n` +
      `Tried:\n` +
      `  KKERNEL_BINARY env var\n` +
      `  @khive/kernel-${platformKey()}/bin/${exe} (npm install)\n` +
      `  ${candidates.join("\n  ")}\n` +
      `If you're developing locally, run: (cd crates && cargo build --release -p kkernel)\n` +
      `Supported platforms: darwin-arm64, darwin-x64, linux-x64-gnu, linux-arm64, win32-x64.`,
  );
}

/**
 * Result of `kkernel sync` — JSON shape from sync::SyncReport in Rust.
 */
export interface SyncReport {
  entities: number;
  edges: number;
  db_path: string;
}

/**
 * Run `kkernel sync` against the given repo and DB target.
 *
 * Throws on non-zero exit code with stderr included in the error message.
 */
export async function runKernelSync(
  repoRoot: string,
  dbPath: string,
  namespace = "local",
): Promise<SyncReport> {
  const bin = kkernelPath(repoRoot);
  const cmd = new Deno.Command(bin, {
    args: ["sync", "--repo", repoRoot, "--db", dbPath, "--namespace", namespace],
    stdout: "piped",
    stderr: "piped",
  });
  const { code, stdout, stderr } = await cmd.output();
  if (code !== 0) {
    const errText = new TextDecoder().decode(stderr);
    throw new Error(`kkernel sync failed (exit ${code}):\n${errText}`);
  }
  const out = new TextDecoder().decode(stdout).trim();
  return JSON.parse(out) as SyncReport;
}
