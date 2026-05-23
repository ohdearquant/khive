# ADR-077: Rust Binary Packaging — Native Per-Platform Subpackages via npm

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-076 (Kernel/MCP Split)

## Context

khive ships two Rust binaries (`kkernel` and `khive-mcp` per ADR-076) and a
Deno CLI (`khive`). Users install the whole stack through npm:

```
npm install -g khive
```

The npm package contains the Deno entry point and shell launcher. The Rust
binaries need to arrive somehow. Three live options for delivering Rust to
JavaScript runtimes:

1. **WASM** (`WebAssembly.instantiate` or `Deno.dlopen` with a `.wasm` blob)
2. **napi-rs** — N-API bindings, distributed via npm
3. **Standalone native binary** invoked as a subprocess

### Why napi-rs is the wrong tool here

napi-rs generates N-API bindings consumable by Node.js. It is excellent for
that. But the khive CLI is **Deno**, not Node — Deno doesn't speak N-API.
Deno has its own FFI (`Deno.dlopen` against cdylibs) and subprocess support
(`Deno.Command`), neither of which uses N-API.

We can borrow napi-rs's **build/packaging infrastructure** (cross-compile
matrix, optional-deps layout) without consuming its FFI layer.

### Why not WASM

The kkernel's hot path is SQLite work (FTS5 trigram indexing, sqlite-vec
vector search) and occasional embedding inference. Concrete blockers:

- **`sqlite-vec` has no upstream WASM build.** It's a C extension; porting is
  non-trivial and we'd own the maintenance.
- **`lattice-embed` uses native BLAS/SIMD.** WASM SIMD is ~3-5× slower than
  native SIMD for embedding workloads.
- **Tokio multi-threaded runtime doesn't exist in WASM.** Only
  `current_thread`. Concurrency on large graphs degrades.
- **Sync throughput** is SQLite-bound; native is 3-10× faster than WASM-SQLite
  per published benchmarks.
- **WASI filesystem**: atomic `tmp+rename` is awkward across host shims.

A WASM fallback for unsupported platforms can be added later as a
`@khive/kernel-wasm` subpackage with reduced functionality. Not in scope here.

### Why subprocess (not Deno FFI)

The kkernel does one-shot operations (sync, migrate, introspect). It's not
hot-path FFI. Subprocess gives:

- Process isolation — kernel crash doesn't take down Deno
- Clean signal handling — kernel can ignore SIGINT until atomic ops finish
- No ABI versioning pain — JSON over stdout/stderr is the contract
- Same model used by deno_task_shell, dprint plugins, wrangler→workerd

## Decision

Ship Rust binaries as **native per-platform npm subpackages**, invoked from the
Deno CLI via `Deno.Command`.

### Package layout

Following the esbuild / swc / biome / prisma pattern:

```
khive (npm main package)
├── package.json (optionalDependencies → platform subpackages)
├── bin/khive            # JS shim that locates the right binary
└── src/                 # Deno TypeScript source
```

Platform subpackages, one per target:

```
@khive/kernel-darwin-arm64   (Apple Silicon macOS)
@khive/kernel-darwin-x64     (Intel macOS)
@khive/kernel-linux-x64-gnu  (Linux x86_64 glibc)
@khive/kernel-linux-x64-musl (Linux x86_64 musl / Alpine)
@khive/kernel-linux-arm64    (Linux ARM64)
@khive/kernel-win32-x64      (Windows x86_64)
```

Each subpackage ships exactly two binaries: `kkernel` and `khive-mcp` for the
matching platform.

### Resolution at runtime

```ts
// cli/lib/kernel.ts (Deno)
function platformKey(): string {
  const os = Deno.build.os;      // "darwin" | "linux" | "windows"
  const arch = Deno.build.arch;   // "x86_64" | "aarch64"
  // Map to npm subpackage naming
  ...
}

export function kkernelPath(): string {
  const key = platformKey();
  const candidates = [
    `${nodeModulesRoot}/@khive/kernel-${key}/bin/kkernel`,
    // fallback paths for dev / monorepo
  ];
  for (const p of candidates) {
    try { Deno.statSync(p); return p; } catch {}
  }
  throw new Error(`No kkernel binary for platform ${key}. ...`);
}
```

### Build/release pipeline

GitHub Actions matrix:

| Job            | Target                    | Builder        |
| -------------- | ------------------------- | -------------- |
| darwin-arm64   | aarch64-apple-darwin      | macos-latest   |
| darwin-x64     | x86_64-apple-darwin       | macos-latest   |
| linux-x64-gnu  | x86_64-unknown-linux-gnu  | ubuntu-latest  |
| linux-x64-musl | x86_64-unknown-linux-musl | ubuntu (cross) |
| linux-arm64    | aarch64-unknown-linux-gnu | ubuntu (cross) |
| win32-x64      | x86_64-pc-windows-msvc    | windows-latest |

Each job runs:

1. `cargo build --release --target <triple> -p kkernel -p khive-mcp`
2. Strip + sign (macOS) / sign (Windows)
3. Publish `@khive/kernel-{platform}@<version>` to npm
4. Wait for all jobs to succeed, then publish the umbrella `khive@<version>`

cargo-zigbuild for clean cross-compile of musl/arm64 targets.

### macOS signing

Unsigned binaries trigger Gatekeeper on first run. CI signs with Apple
Developer ID + notarizes via `xcrun notarytool`. Same path khive already uses
(noted in MEMORY.md).

### Unsupported platforms

If a user installs on Linux riscv64 (or any target not in the matrix), npm
silently skips all optional deps. The Deno CLI then fails at first invocation:

```
Error: No kkernel binary for platform linux-riscv64.
Supported: darwin-arm64, darwin-x64, linux-x64-{gnu,musl}, linux-arm64, win32-x64.
File an issue at https://github.com/ohdearquant/khive/issues if you need this target.
```

Clear failure beats silent fallback to broken behavior.

## Consequences

- **Six native binaries built per release**, two binaries each.
- **Six npm subpackages** plus the umbrella package = seven publishes per
  release. Releases must be atomic — if one subpackage publish fails, others
  must be yanked.
- **No FFI**: kernel state lives in subprocess memory; communication is via
  exit code + JSON on stdout/stderr.
- **Performance**: native — no WASM penalty.
- **CI complexity**: a 6-job release matrix instead of 1. Existing GitHub
  Actions workflow gets extended.
- **Code signing**: required for macOS and Windows distribution at scale.

## Alternatives considered

1. **WASM** — already covered. Performance and dependency-port cost too high
   for v0.1.
2. **napi-rs / Node.js native modules** — wrong runtime; we use Deno.
3. **Deno FFI with cdylib** — works for hot-path APIs but adds ABI versioning
   pain. Our use case is one-shot subprocess invocations where FFI overhead
   is unnecessary. Future hot-path APIs (e.g., live query streaming) may
   revisit this.
4. **Single fat npm package with all binaries** — bloats every install to
   ~80MB. The optional-deps pattern downloads only the matching platform
   (~15MB).
5. **Download-on-first-run via postinstall script** — works but introduces
   network dependency at install time and a different failure mode. npm
   subpackages keep installs offline-friendly once cached.
6. **Distribute via cargo install** — fine for Rust developers; doesn't help
   the JavaScript/Deno-native users that pick khive up via npm.

## Open questions for follow-up

- Should we offer a `@khive/kernel-wasm` fallback for esoteric platforms? Open
  until someone files an issue requesting it.
- Should the umbrella package depend on a specific kernel version, or accept a
  range (`^0.1.0`)? Pin exact match to prevent skew during partial releases.
- Where does the Deno CLI source itself ship — same npm package, or separate?
  Likely same to keep one install.
