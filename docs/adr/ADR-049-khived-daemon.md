# ADR-049: khived daemon — persistent warm runtime over a Unix socket

**Status**: accepted
**Date**: 2026-05-30
**Authors**: Ocean, lambda:khive

## Context

The MCP surface ships as a single binary, `khive-mcp`, launched over **stdio** by each MCP
client (`.mcp.json` → `command: khive-mcp`). Every client reconnect — every `/mcp` reconnect
in Claude Code, every new session — spawns a **fresh process** with an empty in-memory state.

The knowledge pack ([ADR-047](ADR-047-knowledge-pack.md)) serves `knowledge.search` by fusing
FTS5 candidates with a Vamana ANN signal ([ADR-033](ADR-033-vamana-ann.md) family). The ANN
index over the ~466K-vector corpus is held in memory. On a cold process it is rebuilt by
restoring a persisted snapshot (`retrieval_snapshots` BLOB) — today a **~350 MB JSON blob**
that must be read from SQLite, `serde_json`-deserialized, and reconstructed into the graph.

Two defects compound into a "dramatic regression" relative to the pre-OSS khive, which felt
smooth:

1. **Cold start is paid on every reconnect.** Because warm state lives in the process, and the
   process is short-lived, the expensive ANN restore (~50–120 s) recurs indefinitely. There is
   no process that outlives a single client connection.
2. **The restore blocks the first query.** `knowledge.search` calls `ensure_ann().await`
   **inline** before fusing ANN hits. The user's first search hangs for the full restore
   instead of returning the FTS-only result immediately.

The pre-OSS khive-internal solved (1) with a **daemon**: a long-lived process owning the warm
engine, with the CLI as a thin Unix-socket client (`apps/cli/src/server/`). That daemon was
built against the old `StorageBackend` `service.action` dispatch and a large BFF/tenancy/auth
surface that does not belong in OSS. The **pattern** ports; the code does not.

## Decision

Reintroduce a daemon as the warm-state owner, scoped to exactly the piece that fixes the
regression — no HTTP/BFF, no tenancy, no auth plane.

### 1. `khive-mcp --daemon` — one binary, two modes

`khive-mcp` gains a `--daemon` flag. The binary, runtime construction, pack registry, and
config resolution are **identical** in both modes; only the transport differs:

- **default (stdio)** — speaks MCP JSON-RPC over stdio to the client, as today.
- **`--daemon`** — binds a Unix domain socket, builds the same `KhiveRuntime` + `VerbRegistry`,
  warms packs in the background, and serves request frames against that warm registry until
  it receives SIGTERM/SIGINT.

No separate `khived` binary: a single artifact keeps `make local`, packaging, and version
skew trivial. The daemon and the stdio client are guaranteed to share dispatch logic because
they are the same code.

### 2. Thin client + auto-spawn

In stdio mode, the `request` tool handler forwards each call to the daemon instead of
dispatching locally:

```
khive-mcp (stdio, thin)                 khived (khive-mcp --daemon, long-lived)
  request(ops=…)  ──frame──▶  warm VerbRegistry.dispatch ──▶ result
                  ◀─frame───
```

- On the first request, if no responsive socket exists, the client **auto-spawns**
  `current_exe --daemon` detached (own process group, null stdio), inheriting the same env
  (`KHIVE_PACKS`, config path, `HOME`). It polls the socket for readiness (bind happens before
  background warm, so readiness is sub-second), then forwards.
- **Fallback to local dispatch** is mandatory. If the daemon cannot be spawned or reached
  (sandboxed CI, read-only FS, `KHIVE_NO_DAEMON=1`), the client dispatches against its own
  in-process registry — exactly today's behavior. The daemon is an **optimization, never a
  hard dependency**. Tests and the smoke test run daemonless.

### 3. Background lazy warm (smoothness)

Warm becomes **non-blocking**, benefiting both modes:

- `VerbRegistry` gains `async fn call_warm_all(&self)`, mirroring `call_register_embedders`,
  which awaits each pack's existing `PackRuntime::warm()` ([ADR-031] hook, currently an
  unused default no-op).
- The daemon calls `call_warm_all()` in a `tokio::spawn` **after** binding the socket, so the
  socket serves immediately while the ANN warms in the background.
- The knowledge-pack search path stops blocking: the inline `ensure_ann().await` is replaced
  by a **fire-once background warm** (`ensure_ann_background`). Each `knowledge.search` uses
  the ANN signal only if the index is already populated (`ann.read()` is `Some`); until then
  it returns FTS-only results. Once warm completes, subsequent searches fuse ANN automatically.
  Result: no search ever blocks on a rebuild, daemon or not.

### 4. Socket protocol

- Path: `~/.khive/khived.sock`; PID file: `~/.khive/khived.pid` (resolved from `HOME`, like
  the DB path). Both `0600`; socket parent dir `0700`.
- Framing: **length-prefixed** — 4-byte big-endian `u32` length + JSON payload, both
  directions, 8 MiB cap. (Length-prefix, not newline-delimited, because the result payload is
  pretty-printed JSON containing newlines.)
- Request frame: the serialized `RequestParams` (`ops`, `presentation`, `presentation_per_op`)
  plus the client's resolved namespace. Response frame: the JSON string the registry produces
  — byte-identical to what local dispatch returns.
- Lifecycle: on startup, clean up a stale socket/PID (dead PID or unresponsive socket) as the
  old daemon did. On SIGTERM/SIGINT, stop accepting, drain in-flight requests
  (`KHIVE_DRAIN_TIMEOUT_SECS`, default 10), remove socket + PID, exit.

### Scope boundary (what this ADR deliberately excludes)

- No socket auth/admin-token plane. The socket is `0600`, owner-only, loopback-equivalent —
  the same trust boundary as the stdio process it replaces. (The old token plane existed for
  the multi-tenant BFF, which OSS does not ship.)
- No HTTP/SDK listener, no `/api/*`, no tenant registry.
- No change to the snapshot format. Background warm makes the one-time JSON restore invisible;
  a `bincode`/mmap snapshot is a separate, orthogonal optimization (future ADR).
- No multi-namespace daemon. v1 serves the single default namespace its registry was built
  with; the client passes its namespace and the daemon authorizes it per request, but a
  namespace mismatch falls back to local dispatch rather than mis-serving.

## Consequences

**Positive**

- Cold start is paid **once per machine-uptime**, not once per reconnect.
- No search blocks on a rebuild — first query is FTS-instant in either mode.
- Single binary; `make local` unchanged. Daemon is transparent and optional.
- Dispatch logic is shared, so the daemon can never drift from local behavior.

**Negative / risks**

- A long-lived process holding the warm index uses resident memory (~the index size) for the
  machine's session. Mitigated by idle-exit being a cheap future addition; for now the daemon
  exits on signal and is re-spawned on demand.
- Auto-spawn adds a process-management surface (stale socket, zombie daemon). Mitigated by the
  ported cleanup path and the unconditional local-dispatch fallback.
- Config/namespace skew between a stale daemon and a new client. Mitigated by namespace check +
  fallback, and by the daemon being disposable (kill + re-spawn is safe).

## Alternatives considered

1. **Background lazy warm only (no daemon).** Fixes blocking, but every reconnect still re-warms
   from cold. Rejected as the primary fix — it does not address the _repeated_ cost, which is
   the regression.
2. **Faster snapshot (bincode/mmap) only.** Cuts the one-time cost but still pays it per
   reconnect and still blocks inline. Orthogonal; deferred.
3. **Separate `khived` binary.** Cleaner conceptual split, but doubles the build/packaging/
   version surface and risks dispatch drift. Rejected for v1 in favor of one dual-mode binary.

## References

- [ADR-016](ADR-016-request-dsl.md) — request DSL (the forwarded payload boundary)
- [ADR-027](ADR-027-dynamic-pack-loading.md) — pack registry the daemon owns
- [ADR-031](ADR-031-multi-engine-retrieval.md) — `PackRuntime::warm()` / `register_embedders` hooks
- [ADR-047](ADR-047-knowledge-pack.md), ADR-033 family — knowledge search + Vamana ANN
- Pre-OSS reference: `khive-internal/apps/cli/src/server/` (daemon loop), `src/daemon.rs` (client)

## Amendment (2026-06-14): single-binary kkernel topology

The convergence path described in ADR-003 is now complete. The shipped binary is `kkernel`,
not `khive-mcp`. The following topology corrections apply to this ADR.

**Binary name**: The ADR's Context section (line 9) refers to "the MCP surface ships as a
single binary, `khive-mcp`" and describes `.mcp.json → command: khive-mcp`. The shipped
configuration is `command: kkernel` with subcommand `mcp`. The `kkernel` binary is declared
in `crates/kkernel/Cargo.toml` (`[[bin]] name = "kkernel"`, path = `src/main.rs`).

**`khive-mcp` is a library**: `khive-mcp` ships no binary of its own. Its `Cargo.toml`
carries no `[[bin]]` section and its description reads "khive MCP server library — served
via the kkernel binary." Its `lib.rs` (line 4) documents: "The binary frontend is
`kkernel mcp`; this crate ships no binary of its own."

**Daemon spawn**: The Decision section (line 39) describes the daemon flag as
`khive-mcp --daemon`. In the shipped code the daemon is spawned by `spawn_daemon()` in
`crates/khive-mcp/src/daemon.rs` (lines 87-104). The function calls
`std::env::current_exe()` to obtain the running binary path (which resolves to `kkernel`),
then appends the subcommand arguments `["mcp", "--daemon"]`. The MCP entry point
`crates/khive-mcp/src/serve.rs` (line 17) is driven by `kkernel mcp` as confirmed by the
`run(args, registry)` function and the comment on line 3: "This is the bootstrap that the
`kkernel mcp` subcommand drives."

**Section 1 corrected description**: "One binary, two modes" remains accurate in intent,
but the binary is `kkernel`, not `khive-mcp`. The two modes are `kkernel mcp` (stdio) and
`kkernel mcp --daemon` (Unix socket server).

**Diagram correction**: The ASCII diagram in Section 2 should read:

```
kkernel mcp (stdio, thin)              kkernel mcp --daemon (long-lived)
  request(ops=...)  --frame-->  warm VerbRegistry.dispatch --> result
                    <--frame---
```

Rationale: the kkernel unification (ADR-003 convergence path, now complete) absorbed
`khive-mcp` as a library, making `kkernel` the sole shipped Rust binary. All MCP
configurations, daemon-spawn logic, and user-facing documentation should reference
`kkernel mcp` and `kkernel mcp --daemon` instead of `khive-mcp` and `khive-mcp --daemon`.
