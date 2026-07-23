# ADR-026: Rust Binary Packaging via Per-Platform npm Subpackages

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers

## Context

khive's binary topology is set by ADR-003: one Rust binary (`kkernel`), the
`khive-mcp` library served through `kkernel mcp`, and a Deno CLI (`khive`). Users install
the stack through npm:

```
npm install -g khive
```

The npm package contains the Deno entry point and shell launcher. The Rust binaries need
to arrive somehow. Three live options for delivering Rust to JavaScript runtimes:

1. **WASM** (`WebAssembly.instantiate` or `Deno.dlopen` with a `.wasm` blob)
2. **napi-rs**: N-API bindings, distributed via npm
3. **Standalone native binary** invoked as a subprocess

The packaging mechanism must satisfy:

1. **Single `npm install` from the user's perspective.** No "now download the binary" step.
2. **Per-platform native performance.** sqlite-vec, FTS5, embedding inference are hot
   paths; WASM degradation is not acceptable.
3. **Deno-native invocation.** The CLI runs on Deno, not Node; the chosen mechanism must
   work with `Deno.Command`.
4. **Cross-platform coverage.** macOS (Intel and Apple Silicon), Linux (glibc and musl,
   x86_64 and ARM64), and Windows x86_64.

## Decision

Ship Rust binaries as **native per-platform npm subpackages**, invoked from the Deno CLI
via `Deno.Command`. The umbrella `khive` package declares per-platform subpackages as
`optionalDependencies`; npm installs only the one matching the host platform.

### Package layout

Following the esbuild / swc / biome / prisma pattern:

```
khive (npm main package)
├── package.json          # optionalDependencies → platform subpackages
├── bin/khive             # JS shim that locates the matching binary
└── src/                  # Deno TypeScript source
```

Platform subpackages, one per target:

```
khive-kernel-darwin-arm64    (Apple Silicon macOS)
khive-kernel-darwin-x64      (Intel macOS)
khive-kernel-linux-x64-gnu   (Linux x86_64 glibc)
khive-kernel-linux-x64-musl  (Linux x86_64 musl / Alpine)
khive-kernel-linux-arm64     (Linux ARM64 glibc)
khive-kernel-win32-x64       (Windows x86_64)
```

Each subpackage ships the `kkernel` binary for the matching platform. MCP functionality
is reached through the `kkernel mcp` subcommand; `khive-mcp` is a library crate, not a
second distributed binary.

### Resolution at runtime

```ts
// cli/lib/kernel.ts (Deno)
function platformKey(): string {
  const os = Deno.build.os; // "darwin" | "linux" | "windows"
  const arch = Deno.build.arch; // "x86_64" | "aarch64"
  // Map to npm subpackage naming
  // ...
}

export function kkernelPath(): string {
  const key = platformKey();
  const candidates = [
    `${nodeModulesRoot}/khive-kernel-${key}/bin/kkernel`,
    // fallback paths for development / monorepo
    `${projectRoot}/target/release/kkernel`,
  ];
  for (const p of candidates) {
    try {
      Deno.statSync(p);
      return p;
    } catch {}
  }
  throw new Error(
    `No kkernel binary for platform ${key}. ` +
      `Supported: ${SUPPORTED_PLATFORMS.join(", ")}.`,
  );
}
```

The Deno CLI never resolves the binary path at install time: only at first invocation.
This means a `khive` install on an unsupported platform succeeds (npm silently skips
unsatisfiable optional deps) but fails on first CLI command with a clear error message.

### Build / release pipeline

GitHub Actions matrix:

| Job            | Target triple             | Runner                      |
| -------------- | ------------------------- | --------------------------- |
| darwin-arm64   | aarch64-apple-darwin      | macos-latest                |
| darwin-x64     | x86_64-apple-darwin       | macos-latest                |
| linux-x64-gnu  | x86_64-unknown-linux-gnu  | ubuntu-latest               |
| linux-x64-musl | x86_64-unknown-linux-musl | ubuntu (cross via zigbuild) |
| linux-arm64    | aarch64-unknown-linux-gnu | ubuntu (cross via zigbuild) |
| win32-x64      | x86_64-pc-windows-msvc    | windows-latest              |

Each matrix job builds the target binary and performs the signing or notarization
required by that platform. Publication automation is maintainer workflow rather than an
architectural contract. The stable guarantee is that the umbrella package references a
complete, same-version set of supported platform packages.

`cargo-zigbuild` is used for clean cross-compile of musl/arm64 targets: produces working
binaries without per-target Docker images.

### macOS signing and notarization

Unsigned binaries trigger Gatekeeper on first run. CI signs with Apple Developer ID,
notarizes via `xcrun notarytool`, and staples the ticket before publishing. This is a
hard requirement for any deployment that wants Gatekeeper-clean execution.

Windows binaries are signed via Authenticode if a code-signing certificate is configured;
unsigned Windows binaries work but trigger SmartScreen warnings on first run.

### Atomic release semantics

The umbrella package is published only after every supported platform package for the
same version is available. If any platform build or publication fails, the umbrella
package remains at the preceding version. Release recovery and package-revocation
procedures belong in maintainer documentation rather than this architectural contract.

### Unsupported platforms

If a user installs on Linux riscv64 (or any target not in the matrix), npm silently skips
all optional deps and the install completes "successfully" from npm's perspective. The
Deno CLI fails at first invocation:

```
Error: No kkernel binary for platform linux-riscv64.
Supported: darwin-arm64, darwin-x64, linux-x64-{gnu,musl}, linux-arm64, win32-x64.
File an issue at https://github.com/ohdearquant/khive/issues if you need this target.
```

Clear failure beats silent fallback to broken behavior.

### Future WASM subpackage (optional, not v1)

A `khive-kernel-wasm` subpackage could be added later as a fallback for unsupported
platforms, with reduced functionality (no sqlite-vec acceleration, no parallel embed
inference). Not in scope for v1; tracked as a future enhancement.

## Rationale

### Why subprocess, not Deno FFI

The kkernel does one-shot operations (sync, migrate, pack introspection) and one
long-running operation (the MCP server). It is not hot-path FFI. Subprocess gives:

- **Process isolation**: kernel crash does not take down Deno
- **Clean signal handling**: kernel can ignore SIGINT until atomic ops finish
- **No ABI versioning pain**: JSON over stdout/stderr (or stdin/stdout for MCP JSON-RPC)
  is the contract
- **Same model used by deno_task_shell, dprint plugins, wrangler → workerd**: proven
  pattern

Future hot-path APIs (e.g., live query streaming) may revisit this; the v1 surface is
subprocess.

### Why not WASM

The kkernel's hot path is SQLite work (FTS5 trigram indexing, sqlite-vec vector search)
and occasional embedding inference. Concrete blockers:

- **`sqlite-vec` has no upstream WASM build.** It is a C extension; porting is
  non-trivial and we would own the maintenance.
- **The native embedding provider uses BLAS/SIMD.** Its required native execution paths
  are not available with equivalent behavior in the WASM target.
- **Tokio multi-threaded runtime does not exist in WASM.** Only `current_thread`.
  Concurrency on large graphs degrades.
- **Sync throughput** is SQLite-bound; native is 3-10× faster than WASM-SQLite per
  published benchmarks.
- **WASI filesystem**: atomic `tmp+rename` is awkward across host shims.

A WASM subpackage can be added later for esoteric platforms with reduced functionality.
Not the default path.

### Why not napi-rs

napi-rs generates N-API bindings consumable by Node.js. khive's CLI is Deno, not Node;
Deno does not speak N-API. Deno has its own FFI (`Deno.dlopen` against cdylibs) and
subprocess support (`Deno.Command`), neither of which uses N-API.

We can borrow napi-rs's build/packaging infrastructure (cross-compile matrix, optional-
deps layout) without consuming its FFI layer. That is exactly what this ADR does: the
optional-subpackage pattern is napi-rs's; the binary execution model is Deno-native.

### Why optionalDependencies, not postinstall download

`optionalDependencies` in npm:

- **Offline-friendly once cached**: npm caches the matching subpackage; subsequent
  installs work without network.
- **No postinstall scripts**: security-sensitive environments (corporate, CI) often
  disable postinstall scripts; optional deps work anyway.
- **Atomic from user's perspective**: `npm install -g khive` either succeeds with a
  working binary or fails cleanly.

Postinstall download has none of these properties and adds a different failure mode
(network outage during install).

### Why per-platform subpackage, not fat umbrella

A single fat npm package containing all binaries would bloat every install to ~80 MB. The
optional-deps pattern downloads only the matching platform (~15-20 MB). For a CLI that
users install once and run constantly, install size is a real friction point.

## Alternatives Considered

| Alternative                           | Pros                           | Cons                                                         | Why rejected                                   |
| ------------------------------------- | ------------------------------ | ------------------------------------------------------------ | ---------------------------------------------- |
| WASM                                  | One binary, all platforms      | Performance + dependency-port cost too high                  | Doesn't meet hot-path requirements             |
| napi-rs / Node native modules         | Mature ecosystem               | Wrong runtime; we use Deno                                   | Deno doesn't speak N-API                       |
| Deno FFI with cdylib                  | Lower overhead than subprocess | ABI versioning pain; our use case is one-shot invocations    | Subprocess fits the pattern                    |
| Single fat npm package                | One artifact                   | ~80 MB install for every user                                | Optional-deps pattern is ~15-20 MB             |
| Download-on-first-run via postinstall | Smallest install               | Network dependency at install; different failure mode        | Optional-deps is offline-friendly once cached  |
| Distribute via cargo install          | Fine for Rust developers       | Doesn't help the Deno-native users who pick khive up via npm | Doesn't meet "single npm install" requirement  |
| Brew + apt + chocolatey + cargo       | Native package managers        | Tripled release surface; per-platform packaging work         | One pipeline, multiple subpackages, is simpler |

## Consequences

### Positive

- **Single `npm install -g khive`** delivers a working stack on every supported platform.
- **Native performance**: no WASM penalty on hot paths.
- **Install size ~15-20 MB** per user, not ~80 MB.
- **Offline-friendly**: npm caches the subpackage; subsequent installs work without
  network.
- **CI signing**: macOS and Windows binaries pass Gatekeeper / SmartScreen cleanly.

### Negative

- **A platform matrix is required.** Each supported target needs a native build and
  verification job. `cargo-zigbuild` handles the musl and ARM64 cross-compilation paths.
- **Code signing required**: Apple Developer ID + Windows Authenticode certificates are
  ongoing operational costs.
- **No FFI**: kernel state lives in subprocess memory; communication is via exit code +
  JSON on stdout/stderr or JSON-RPC on stdin/stdout.

### Neutral

- **`cargo install kkernel`** still works for Rust developers who want the binary
  directly. The npm path is for Deno-native users.
- **MCP wire protocol unchanged**: khive-mcp speaks the same stdio JSON-RPC as a
  cargo-installed binary.

## Open Questions

1. **WASM fallback subpackage**. Open until someone files an issue requesting a target
   not in the matrix (e.g., riscv64, FreeBSD). When that happens, build a reduced-
   functionality WASM subpackage as `khive-kernel-wasm` and document the trade-offs.
2. **Umbrella → subpackage version pinning**. Pin exact match (`khive-kernel-* === khive
   version`) to prevent skew during partial releases. Accepting a range (`^0.1.0`) would
   allow security patches without umbrella republish but breaks the atomic-release model.
   v1: exact pin.
3. **Where does the Deno CLI source ship**. Same npm package, or separate? Default: same,
   to keep one install. A `khive-cli` separate package would only matter if we wanted to
   version the CLI independently of the kernel, which we do not yet.

## References

- [ADR-003](./ADR-003-system-architecture.md): single-binary `kkernel` topology and
  `kkernel mcp`
- esbuild's per-platform subpackage pattern: https://esbuild.github.io/getting-started/#install-esbuild
- napi-rs cross-compile matrix: https://napi.rs/docs/cli/build
- `cargo-zigbuild`: https://github.com/rust-cross/cargo-zigbuild
- Apple notarization: `xcrun notarytool`
- Windows Authenticode signing
