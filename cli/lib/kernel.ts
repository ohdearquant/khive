/**
 * Resolve the path to the `kkernel` Rust binary (ADR-026).
 *
 * Strategy (in order):
 *   1. `KKERNEL_BINARY` env var — explicit override, used in dev and tests.
 *   2. `khive-kernel-<platform>/bin/kkernel` under node_modules — production
 *      install via npm optional dependencies (ADR-026).
 *   3. `<repo>/crates/target/release/kkernel` — monorepo dev convenience.
 *   4. `<repo>/crates/target/debug/kkernel` — last-resort dev fallback.
 *
 * Throws a descriptive error when no candidate exists.
 */

import { dirname, fromFileUrl, join } from "@std/path";

/**
 * Detect whether the Linux runtime links against musl (Alpine etc.) or glibc.
 * Returns "gnu" or "musl". Defaults to "gnu" if detection is inconclusive.
 *
 * Detection order (most-reliable first):
 *   1. `ldd --version` — invokes the actual system linker (same as the Node
 *      shim in npm/bin/khive). More reliable than /proc/self/maps which
 *      reflects the Deno process's own loader, not the child binary's.
 *   2. `/lib/ld-musl-*` glob — fast filesystem check, no subprocess.
 *
 * NOTE: npm/bin/khive and npm/bin/khive-mcp use the same ordered detection.
 * Keep all three in sync.
 */
function detectLibc(): "gnu" | "musl" {
  try {
    const result = new Deno.Command("ldd", {
      args: ["--version"],
      stdin: "null",
      stdout: "piped",
      stderr: "piped",
    }).outputSync();
    const out = new TextDecoder()
      .decode(result.stdout)
      .toLowerCase()
      .concat(new TextDecoder().decode(result.stderr).toLowerCase());
    if (out.includes("musl")) return "musl";
    return "gnu";
  } catch {
    // ldd not available — fall through
  }
  try {
    for (const entry of Deno.readDirSync("/lib")) {
      if (entry.name.startsWith("ld-musl-")) return "musl";
    }
  } catch {
    // /lib not readable — fall through
  }
  return "gnu";
}

/**
 * Resolve the platform suffix for the khive-kernel-{platform} subpackage on
 * Linux. Returns the suffix string, or throws with a clear "unsupported"
 * message for musl arm64 (not in the v1 matrix).
 */
function linuxVariant(arch: "x86_64" | "aarch64"): string {
  const libc = detectLibc();
  if (arch === "aarch64") {
    if (libc === "musl") {
      throw new Error(
        "khive does not support linux-arm64 with musl libc in v1.\n" +
          "linux-arm64 with musl is not in the v1 release matrix.\n" +
          "Supported: darwin-arm64, darwin-x64, linux-x64-gnu, linux-x64-musl, linux-arm64 (glibc), win32-x64.\n" +
          "File an issue at https://github.com/ohdearquant/khive/issues if you need this target.",
      );
    }
    return "linux-arm64";
  }
  return libc === "musl" ? "linux-x64-musl" : "linux-x64-gnu";
}

function platformKey(): string {
  const os = Deno.build.os;
  const arch = Deno.build.arch;
  if (os === "linux") return linuxVariant(arch as "x86_64" | "aarch64");
  const map: Record<string, string> = {
    "darwin-aarch64": "darwin-arm64",
    "darwin-x86_64": "darwin-x64",
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
      `khive-kernel-${platformKey()}`,
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
      `  khive-kernel-${platformKey()}/bin/${exe} (npm install)\n` +
      `  ${candidates.join("\n  ")}\n` +
      `If you're developing locally, run: (cd crates && cargo build --release -p kkernel)\n` +
      `Supported platforms: darwin-arm64, darwin-x64, linux-x64-gnu, linux-x64-musl, linux-arm64, win32-x64.`,
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
