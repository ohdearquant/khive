# ADR-049: khived daemon — persistent warm runtime over a Unix socket

**Status**: accepted
**Date**: 2026-05-30
**Authors**: khive maintainers
**Amended by**: [ADR-067](ADR-067-write-owner-daemon.md), which adds the single-writer
queue, and [ADR-096](ADR-096-warm-daemon-per-request-identity.md), which permits
per-request identity in one warm daemon.

## Context

The MCP surface ships as a single binary, `khive-mcp`, launched over **stdio** by each MCP
client (`.mcp.json` → `command: khive-mcp`). Every client reconnect — every `/mcp` reconnect
in Claude Code, every new session — spawns a **fresh process** with an empty in-memory state.

The knowledge pack ([ADR-047](ADR-047-knowledge-pack.md)) serves `knowledge.search` by fusing
FTS5 candidates with a Vamana ANN signal ([ADR-052](ADR-052-ann-production-lifecycle.md)
family). The ANN
index over the ~466K-vector corpus is held in memory. On a cold process it is rebuilt by
restoring a persisted snapshot (`retrieval_snapshots` BLOB) — today a **~350 MB JSON blob**
that must be read from SQLite, `serde_json`-deserialized, and reconstructed into the graph.

Two defects compound into a "dramatic regression" relative to the earlier implementation, which felt
smooth:

1. **Cold start is paid on every reconnect.** Because warm state lives in the process, and the
   process is short-lived, the expensive ANN restore (~50–120 s) recurs indefinitely. There is
   no process that outlives a single client connection.
2. **The restore blocks the first query.** `knowledge.search` calls `ensure_ann().await`
   **inline** before fusing ANN hits. The user's first search hangs for the full restore
   instead of returning the FTS-only result immediately.

The earlier implementation solved (1) with a **daemon**: a long-lived process owning the warm
engine, with the CLI as a thin Unix-socket client (`apps/cli/src/server/`). That daemon was
built against the old `StorageBackend` `service.action` dispatch and a large BFF/tenancy/auth
surface that is not part of this codebase. The **pattern** ports; the code does not.

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
  the same trust boundary as the stdio process it replaces.
- No HTTP/SDK listener, no `/api/*`.
- No change to the snapshot format. Background warm makes the one-time JSON restore invisible;
  a `bincode`/mmap snapshot is a separate, orthogonal optimization (future ADR).
- No multi-namespace daemon. v1 serves the single default namespace its registry was built
  with; the client passes its namespace and the daemon authorizes it per request, but a
  namespace mismatch falls back to local dispatch rather than mis-serving.
  Amended by [ADR-096](ADR-096-warm-daemon-per-request-identity.md): the daemon may serve
  per-request attribution identities over one shared backend; see that ADR for the
  per-request identity model and its acceptance conditions.

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
- Earlier reference: `apps/cli/src/server/` (daemon loop), `src/daemon.rs` (client)

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

## Amendment 2 (2026-07-14): graduated fail-loud fallback policy

This amendment supersedes the unconditional fallback mandate in Decision section 2
("**Fallback to local dispatch** is mandatory. ... The daemon is an **optimization, never a
hard dependency**."). Operational experience showed that an unconditional silent fallback
masks exactly the defect classes the daemon rollout must surface: configuration divergence
between client and daemon, namespace mismatches, version skew after a binary upgrade, and
broken daemon recovery (a spawn that fails, or a spawned child that exits before binding the
socket). A client that silently serves such a request from its cold in-process registry
reports success while hiding a topology fault.

### Graduated policy

Every fallback decision is classified by a closed `FallbackReason` set with stable string
codes: `config_mismatch`, `namespace_mismatch`, `no_socket`, `parse_failure`,
`protocol_mismatch`. Reasons carry a legitimacy tier that governs observability:

- **No-daemon** (`no_socket`): a daemon is genuinely absent or unreachable. Fallback to
  local dispatch proceeds quietly, as originally specified. Environments that cannot or
  must not run a daemon opt out explicitly with `KHIVE_NO_DAEMON=1`, which short-circuits
  before any spawn attempt; tests and the smoke test continue to run daemonless.
- **Rollout-transient** (`protocol_mismatch`, `parse_failure`): expected briefly during
  binary upgrades. Fallback proceeds, logged at WARN with the reason code.
- **Illegitimate** (`config_mismatch`, `namespace_mismatch`): a correctly configured
  deployment never produces these. Fallback is logged at ERROR and counted in a dedicated
  strict-violations metric alongside the per-reason fallback counters.

Amendment 3 supersedes the rollout behavior in the second bullet: the two codes remain in the
metrics vocabulary, but a post-write `protocol_mismatch` or `parse_failure` is now terminal and
never falls back or enters daemon recovery.

Under `KHIVE_DAEMON_STRICT=1`, a request that would fall back for **any** reason is instead
rejected with a structured error carrying the reason code and a stable machine-checkable
marker, so "strict mode active and fallback count zero" is a sound proof that every served
request was daemon-dispatched. Strict mode is default-off.

### Confirmed respawn failures reject

When the client itself initiated daemon recovery and **positively observed** the failure —
the spawn call returned an error, or the spawned child exited before binding the socket —
the failure is a confirmed daemon-recovery fault, not a no-daemon environment. These paths
do not fall back in either mode: they return a stable `respawn_failed` error with safe
remediation text. Detailed context (spawn error, log excerpts, executable path) is emitted
through local structured logging only and never included in the caller-visible error.

Rationale: a confirmed respawn failure means the operator's installation is broken (bad
binary, permissions, version skew). Serving the request cold would hide that fault behind
degraded-but-working behavior; the explicit opt-outs (`KHIVE_NO_DAEMON=1` for daemonless
environments) remain the sanctioned way to run without a daemon.

### What is unchanged

The daemon remains an optimization for warm-state reuse; the thin-client architecture,
socket protocol, and background warm behavior of this ADR are unaffected. Quiet local
dispatch remains the contract for genuinely daemonless environments (`no_socket` without a
confirmed failed recovery attempt, and the `KHIVE_NO_DAEMON=1` opt-out).

## Amendment 3 (2026-08-01): exactly-once recovery boundary and parallel convergence

This amendment corrects Amendment 2's rollout-transient description after the exactly-once
boundary introduced by #644. `NoSocket` is the only forward outcome eligible to enter daemon
recovery: connection establishment or the frame write failed, so the request is proven not to
have reached dispatch. A `ParseFailure` or `ProtocolMismatch` observed after the real frame was
fully written is terminal. The client returns a hard error and performs no retry, local dispatch,
kill, or respawn, because the mutation may already have committed before its response was lost.
The two reason codes remain reserved in the closed fallback-metrics vocabulary for compatibility;
they are not production local-fallback events.

Parallel `NoSocket` recovery is required to converge after quiescence to exactly one live,
identity-matching daemon owning the socket/PID rendezvous. The client-side recoverer lock
serializes confirmation and launch decisions, while the daemon-side boot fence chooses the sole
owner. The contract does **not** require exactly one launch attempt: the recovery lock can be
released before a launched child binds, so another caller may legitimately attempt a launch that
later loses the daemon-side fence.

The executable gate uses eight synchronized clients. One scenario begins from `NoSocket`, runs the
real daemon server through the test launcher seam on a multi-thread runtime, and requires one live
owner plus a successful `stats()` exchange and complete teardown. The other makes all eight clients
lose their response only after the real frame is read and requires eight terminal ambiguity errors,
zero lifecycle actions, and no follow-up connections. Both scenarios repeat 25 times on every CI
operating system. Shared recovery counters, framing fixtures, environment cleanup, and the
in-process launcher live in `crates/khive-mcp/src/daemon/test_harness.rs`.

## Amendment 4 (2026-08-09): an inaccessible socket is not an absent daemon

This amendment narrows Amendment 2's phrase "genuinely absent or unreachable" and Amendment 3's
generic reference to connection-establishment failure. Only an error that positively establishes
there is no live listener is a recoverable `NoSocket` outcome:

- `ENOENT` / `NotFound` means no socket path exists.
- `ECONNREFUSED` / `ConnectionRefused` means a stale socket path exists with no listener.

Both remain eligible for the ordinary `NoSocket` recovery path, including stale-socket self-heal.
Any other connect error, including sandbox or filesystem-policy denial reported as `EACCES` or
`EPERM` (`PermissionDenied`), is `Unreachable`. It proves only that this client cannot inspect the
daemon rendezvous; it does not prove the daemon is dead. `Unreachable` therefore returns a
structured `daemon_unreachable` hard error and performs no local dispatch, retry, kill, or spawn,
regardless of `KHIVE_DAEMON_STRICT`.

`KHIVE_NO_DAEMON=1` remains the explicit operator opt-out for intentionally daemonless execution.
This amendment changes neither that opt-out nor the exactly-once boundary after a frame write.

## Amendment 5 (2026-08-11): disconnect propagation and bounded request drain

Each accepted connection now owns a retained handler `JoinHandle` and a
per-daemon-run read-cancellation receiver. After its single request frame is
read, the daemon monitors the peer read half concurrently with dispatch. EOF,
reset, or unexpected additional bytes signals the request's read scopes; an
already-admitted write is still awaited to its normal commit/rollback boundary.
The client-side MCP forwarding path likewise closes the stream when its rmcp
cancellation token or original absolute deadline fires, so the daemon never
renews a disconnected client's budget.

The stdio transport itself shares one root cancellation token with rmcp. Its
EOF adapter cancels that token before returning EOF to rmcp, which cancels
every admitted per-request child before rmcp begins its graceful response
drain. On Unix this adapter wraps the existing post-flush self-heal transport:
successful response flushes still fire the armed action at the same
happens-after edge, and resumed generations still use the handshake-free
`serve_directly` path. The non-Unix handshake path uses the same EOF adapter.
EOF only abandons request-owned reads; admitted writes keep their existing
commit/rollback boundary.

Shutdown stops acceptance, signals every run-local read scope, and permits
admitted handlers to finish inside `KHIVE_DRAIN_TIMEOUT_SECS`. Connection
handles are retained rather than detached. At the bound, remaining async
handlers are aborted and every join is drained before rendezvous cleanup; a
dropped read future fires its exact SQLite interrupt guard. The historical
process-global one-shot shutdown token remains component coordination only and
is not consulted as request cancellation state, preserving repeated daemon-run
tests. A blocking write that ignores async abort continues to own its SQLite
connection until its blocking closure exits; the daemon never interrupts that
write or reports a retryable read timeout for it.

## Amendment 6 (2026-08-26): incumbent classification at boot

Boot answers one question — "may I clean up the rendezvous and bind?" — and the
convergence invariant above requires that the answer be governed by whether a
daemon is **live**, never by whether it is one this process would choose to talk
to. This amendment separates those two questions and enumerates the states boot
must distinguish.

### Liveness and acceptability are different questions

- **Liveness**: is a process serving this socket right now? Answered by the
  connect and by whether anything comes back at all.
- **Acceptability**: is that process a peer this client can share the rendezvous
  with? Answered by identity — protocol version and `config_id`.

Unlink-and-rebind is licensed by a negative answer to **liveness only**. It is
never licensed by a negative answer to acceptability, because an unacceptable
peer that is alive still owns the socket. Removing its rendezvous files does not
stop it, and binding a second listener beside it violates the "exactly one live,
identity-matching daemon" convergence requirement directly.

A probe predicate that returns a bare boolean invites the collapse, because a
single bit cannot carry the difference between "nothing is there" and "something
is there that I may not use". Boot classification therefore returns a
disposition, not a boolean:

```
MayBind
Refuse { pid: Option<u32>, reason: RefusalReason }
```

`MayBind` is the only value that permits binding a listener.

### Incumbent states

`PID live` means the recorded pid names a running process. `connect` is the
bounded probe connect. `response` is what came back within the probe deadline.

Exit codes are normative because supervisors read them: a supervisor configured
to restart only on unsuccessful exit treats exit 0 as a deliberate stop.
**Configuration-class** refusals — an operator must act, and restarting cannot
help — exit 0. **State-class** refusals — transient or unclassified, where a
retry may succeed — exit nonzero.

| #  | State                                         | Observable                                                | Disposition                                                                                   | Exit    |
| -- | --------------------------------------------- | --------------------------------------------------------- | --------------------------------------------------------------------------------------------- | ------- |
| 1  | Exact acknowledgement                         | connect ok, probe ack, identity matches                   | Refuse to start; report the incumbent pid                                                     | 0       |
| 2  | Protocol-version mismatch                     | connect ok, `version_mismatch`                            | Refuse; name both versions and the remedy. Never unlink                                       | 0       |
| 3  | Config mismatch                               | connect ok, `config_mismatch`, `served_config_id` present | Refuse by default; name the first differing field without values. Opt-in replacement only     | 0       |
| 4  | Metrics-only reply                            | connect ok, reply carries metrics                         | Refuse to start                                                                               | 0       |
| 5  | Silent connect                                | connect ok, no parseable response within the deadline     | Refuse; distinct error "connected but did not answer"                                         | nonzero |
| 6  | Malformed reply                               | connect ok, bytes returned, frame does not parse          | Refuse to start                                                                               | nonzero |
| 7  | Connection refused, pid live                  | `ECONNREFUSED`, pid running                               | Refuse; name the pid                                                                          | nonzero |
| 8  | Connection refused, pid dead                  | `ECONNREFUSED`, pid not running                           | Clean up and bind                                                                             | —       |
| 9  | No socket, pid dead                           | socket absent, pid not running or pid file absent         | Clean up and bind                                                                             | —       |
| 10 | No socket, pid live                           | socket absent, pid running                                | Refuse; name the pid, unless the pid is this process and same-process incumbency is permitted | 0       |
| 11 | Other connect error (`EACCES`, `ENOTSOCK`, …) | connect fails, not refused                                | Refuse; name the errno                                                                        | nonzero |

Only states 8 and 9 are genuinely stale. Collapsing any of 2, 3, 5, 6, 7, 10 or
11 into "clean up and bind" is the defect this amendment forbids.

Every state above is reachable from the boot probe except **state 4**. The boot
probe's request frame does not set the metrics flag, and the daemon's
metrics reply is gated on that flag in the requesting frame, so a metrics-only
reply cannot be elicited by boot. State 4 is retained as a defensive
classification because a reply carrying metrics is unambiguous proof of life
whatever elicited it, and misreading it as absence would be the same defect.

Two consequences of the existing contract are worth stating because they are
easy to invert:

- **State 3 is not automatic replacement.** The Consequences section's
  description of the daemon as disposable, in the sense that killing and
  respawning it is safe, licenses replacement being _safe_. It does not make
  replacement something boot performs unprompted. Disposable does not mean
  auto-replaced at boot.
- **State 10 is a clean exit.** The convergence requirement already anticipates
  multiple launch attempts and lets the daemon-side fence choose the sole owner,
  so losing that fence is legitimate rather than an error. Blocking and
  re-probing belong to the client-side recoverer, never to daemon boot.

### Replacing a live incumbent

Removal of a live incumbent is operator-elected and available only through an
explicit `--replace-incumbent` opt-in. Every step is required:

1. Classify. Proceed only from states 1 through 4, where a daemon identity was
   positively read. Never from 5, 6, 7, 10 or 11 — those have not established
   what would be killed.
2. Signal `SIGTERM` to the recorded pid.
3. Wait, bounded, for process death. Death is **observed**, never assumed.
4. On timeout, refuse: leave the socket intact and name the pid. Never escalate
   to `SIGKILL` implicitly, and never unlink a socket whose owner is still
   alive.
5. Only after observed death, remove the socket and pid file, then bind.

Step 3 is the substance. Removing a rendezvous file is not a way to stop a
process.

### Test obligations

One test per state, each asserting the **outcome** rather than the return value.
For every refuse state that must assert: the incumbent is still alive, the
socket still exists, and no second listener was bound. A test that checks only
the returned error passes while the socket is unlinked underneath a live
process, which is precisely the failure being prevented. Each refuse state also
asserts its exit code, since an otherwise-correct refusal carrying the wrong
code either drives a supervisor restart loop or silently retires a retryable
state.

Two further tests are required because their absence is what allowed the
collapse:

- A test that starts a real live incumbent and drives each non-acknowledgement
  class against it, asserting that startup never leaves two live daemons.
- A `--replace-incumbent` timeout test in which the incumbent ignores `SIGTERM`,
  asserting refusal with the socket intact.

Each state's guard carries a mutation control: defeat the guard, confirm that
exactly that state's test fails, and restore from a snapshot rather than by
reapplying an inverse edit.

### What is unchanged

The convergence requirement, the client-side recoverer lock, the daemon-side
boot fence, the exactly-once recovery boundary of Amendment 3, and the socket
accessibility narrowing of Amendment 4 all stand as written. This amendment
constrains only what boot may conclude from a probe result, and what it may do
about it.
