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
- Lifecycle: on startup, clean up a stale socket/PID. **"Stale" is defined by the
  incumbent state table in Amendment 6 and nowhere else**: only the states that
  table marks "clean up and bind" are stale. In particular a socket that accepts
  a connection and then stays silent is NOT stale — it is a live peer that did
  not answer, and the amendment requires refusing and leaving the rendezvous
  intact. The earlier phrasing of this rule, "dead PID or unresponsive socket",
  is superseded: it reads a connected-but-silent peer as cleanable, which is the
  exact collapse the amendment forbids. On SIGTERM/SIGINT, stop accepting, drain
  in-flight requests (`KHIVE_DRAIN_TIMEOUT_SECS`, default 10), remove socket +
  PID, exit.

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

**This amendment specifies a contract that current code does not satisfy.** It
is normative, not descriptive: every rule below states what boot must do, and
none of it should be read as an account of what boot does today. The present
implementation reduces the probe to a boolean that is true only for an exact
acknowledgement, and treats everything else — including replies from
demonstrably live daemons — as grounds for removing the rendezvous. Bringing the
code to this contract is a prerequisite for the guarantees stated here, not a
consequence of recording them.

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

**The class rule is "who resolves it", not "how long it lasts".** A refusal
exits 0 when the condition can only be resolved by something other than a later
retry of this process — an operator changing configuration, or another process
that already owns the rendezvous. A refusal exits nonzero when the resolver IS a
later retry of this process, so that a supervisor restart is the remedy.

Transience is not the test. A rollout in which an older daemon answers the probe
can last hours and still exits 0, because no number of restarts of this process
ends it. A peer caught between fork and bind lasts milliseconds and also exits
0, because the other process is the one that resolves it. Conversely a reply
that failed to parse may be a one-off and exits nonzero, because retrying is
exactly what might succeed.

**Codes are numeric and distinct, not merely "nonzero".** A supervisor and an
end-to-end test both need to tell these apart, and "nonzero" is a class, not a
value:

| Exit | Meaning                                                            | States                    |
| ---- | ------------------------------------------------------------------ | ------------------------- |
| 0    | Refused; resolver is an operator or another process                | 1, 2, 3, 4, 6, 12, 13, 14 |
| 1    | Reserved for unclassified failure; never emitted by classification | —                         |
| 2    | Refused; connected but the peer did not answer                     | 7                         |
| 3    | Refused; the peer's reply did not parse                            | 8                         |
| 4    | Refused; connection refused while the recorded pid is running      | 9                         |
| 5    | Reserved; formerly state 13, retired by this amendment             | —                         |
| 6    | Refused; the peer answered but its identity could not be resolved  | 5                         |

Exit 1 is reserved so that an unclassified crash can never be mistaken for a
classified refusal. States 10 and 11 have no exit code because they proceed to
bind rather than refusing.

**State 13 moved from exit 5 to exit 0, and exit 5 is retired rather than
reused.** An earlier draft gave state 13 a nonzero code on the reasoning that a
connect failure is a fault. That contradicted two rules of this document at
once. Amendment 4 classifies every non-`ENOENT`, non-`ECONNREFUSED` connect
error — `EACCES` and `EPERM` among them — as `Unreachable` and forbids retry,
kill, and spawn on it; the class rule above assigns nonzero precisely when a
later retry of this process is the resolver. A sandbox or filesystem-policy
denial is resolved by an operator changing that policy, and no number of
supervisor restarts ends it, so exit 0 is what the class rule requires. Exit 5
is left defined and unemitted, like exit 1, so that a supervisor keyed on the
old value reads a retired code rather than silently inheriting a reused one.

| #  | State                                             | Observable                                                                                                                                            | Disposition                                                                                                                                                         | Exit |
| -- | ------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---- |
| 1  | Exact acknowledgement                             | connect ok, probe ack, identity matches                                                                                                               | Refuse to start; report the incumbent pid                                                                                                                           | 0    |
| 2  | Protocol-version mismatch                         | connect ok, `version_mismatch`                                                                                                                        | Refuse; name both versions and the remedy. Never unlink                                                                                                             | 0    |
| 3  | Config mismatch                                   | connect ok, `config_mismatch` set — `served_config_id` may be absent, and is not part of this predicate                                               | Refuse by default; name the first differing field without values when `served_config_id` is present, otherwise say identity was not echoed. Opt-in replacement only | 0    |
| 4  | Metrics-only reply                                | connect ok, reply carries metrics                                                                                                                     | Refuse to start                                                                                                                                                     | 0    |
| 5  | Identity unresolved                               | connect ok, reply HAS ack shape, no mismatch flag set, `served_config_id` absent or unequal                                                           | Refuse to start; name the pid and that identity could not be established                                                                                            | 6    |
| 6  | Parseable non-acknowledgement                     | connect ok, frame deserializes, reply does NOT have ack shape                                                                                         | Refuse to start; name the observed shape                                                                                                                            | 0    |
| 7  | Silent connect                                    | connect ok, no parseable response within the deadline                                                                                                 | Refuse; distinct error "connected but did not answer"                                                                                                               | 2    |
| 8  | Malformed reply                                   | connect ok, bytes returned, frame does not parse                                                                                                      | Refuse to start                                                                                                                                                     | 3    |
| 9  | Connection refused, pid live                      | `ECONNREFUSED`, pid running                                                                                                                           | Refuse; name the pid                                                                                                                                                | 4    |
| 10 | Connection refused, no live listener              | `ECONNREFUSED`, and the recorded pid is not running OR no usable pid record exists (file absent, empty, or not parseable as a pid)                    | Clean up and bind                                                                                                                                                   | —    |
| 11 | No socket, pid dead                               | socket absent, pid not running or pid file absent                                                                                                     | Clean up and bind                                                                                                                                                   | —    |
| 12 | No socket, pid live                               | socket absent, pid running                                                                                                                            | Refuse; name the pid AND its command line. If the pid is this process and same-process incumbency is permitted, the disposition is `MayBind` instead                | 0    |
| 13 | Other connect error (`EACCES`, `ENOTSOCK`, …)     | connect fails, not refused                                                                                                                            | Refuse; name the errno. Never retry, kill, or spawn (Amendment 4)                                                                                                   | 0    |
| 14 | Namespace mismatch (**defensive**, not reachable) | connect ok, `namespace_mismatch` — no conforming daemon sets this since ADR-096 Fork 1, and a peer old enough to set it fails the version check first | Refuse; name both namespaces. Never unlink, never replace                                                                                                           | 0    |

Only states 10 and 11 are genuinely stale. Collapsing any other state into
"clean up and bind" is the defect this amendment forbids.

**Ack shape is syntactic and is defined independently of identity.** A reply
"has ack shape" when, and only when, `ok` is set and every one of `result`,
`error`, `metrics`, and `request_id` is absent. Nothing about protocol version,
namespace, or `config_id` participates in that test. Identity is evaluated
separately, and only on replies that already have ack shape. Keeping the two
apart is what makes states 5 and 6 distinguishable at all; a definition of
"shape" that folded identity into it would make state 5 unreachable, because
every identity failure would also read as a shape failure.

**Precedence — evaluation order, which row numbers do NOT determine.** Row
numbers in the table above are stable labels, not an order. A single observation
can satisfy more than one row, so classification takes the FIRST match in this
order:

1. No socket → state 11 or 12, by whether the recorded pid is running.
2. Connect fails → state 9, 10, or 13, by errno and pid record.
3. Connect succeeds, nothing parseable within the deadline → state 7.
4. Bytes returned that do not deserialize → state 8.
5. Frame deserializes and either carries a mismatch flag or reports a
   `daemon_protocol_version` unequal to this build's → state 2 (version), 14
   (namespace), or 3 (config). A reply carrying more than one flag is classified
   by the first of those three that is set, because version mismatch subsumes
   the rest: a peer on another protocol cannot be trusted to have evaluated
   namespace or config at all.

   **An unequal `daemon_protocol_version` is state 2 even when
   `version_mismatch` is unset.** The two are independent fields on the
   response, so an older peer that does not recognise this build's version can
   answer ack-shaped, carrying the current `config_id`, no flag set, and its own
   lower version. The current client already carries a separate guard for that
   exact shape (`crates/khive-mcp/src/daemon.rs`), so it is observed rather than
   hypothetical. Routing it to state 5 would call a positively refuted protocol
   identity an unresolved configuration identity, and would give it a
   retry-resolved exit code when no retry of this process can raise the peer's
   version.
6. Frame deserializes, has ack shape, identity matches → state 1.
7. Frame deserializes, carries metrics → state 4.
8. Frame deserializes, has ack shape, no mismatch flag, identity absent or
   unequal → state 5.
9. Frame deserializes, does not have ack shape → state 6. This is the catch-all
   and it is evaluated LAST among response rows. It must never fall through to
   cleanup.

State 5 is evaluated before the state 6 catch-all deliberately. Under a naive
first-match-by-row-number rule the catch-all would absorb every state-5 reply,
because a reply with no `served_config_id` can be described as "not an
acknowledgement" — and a conforming implementation could then never produce the
state-5 diagnosis the table promises.

**States 5 and 6 are the ones an enumeration drawn from a single build will
miss**, so they are stated explicitly, along with what actually produces them.

State 6's motivating case is a live peer that answers with something well-formed
that is not an acknowledgement. **It is NOT produced by the daemon builds that
precede the probe frame in this repository, and an earlier draft of this
amendment said otherwise.** The pre-probe daemon declares `PROTOCOL_VERSION = 1`
and its request handler tests the frame's protocol version FIRST, returning a
`version_mismatch` response before the namespace check and before any dispatch;
the probing build declares version 4. A version-4 probe against that peer
therefore produces **state 2**, not state 6, and a test fixture built from the
earlier description would have been testing a peer that does not exist. The
state-6 fixture must instead be a peer speaking the SAME protocol version that
does not recognise `probe_only` — it deserializes the frame, ignores the unknown
field, dispatches the empty operation string, and returns an ordinary success or
error response. That reply is well-formed, comes from a demonstrably live
process, and is not an acknowledgement, so without state 6 a conforming
implementation has no disposition for it and may classify it as stale.

State 5 covers the same hazard on the identity axis: a reply that has ack shape
but whose `served_config_id` is absent or does not match, while no mismatch flag
is set. Identity that cannot be established is not identity that was refuted,
and neither is death. It exits nonzero rather than 0 because a peer that has not
yet published its identity may publish it before the next attempt, which makes a
retry of this process the resolver.

Every state above is reachable from the boot probe except **states 4 and 14**.
The boot probe's request frame does not set the metrics flag, and the daemon's
metrics reply is gated on that flag in the requesting frame, so a metrics-only
reply cannot be elicited by boot. State 4 is retained as a defensive
classification because a reply carrying metrics is unambiguous proof of life
whatever elicited it, and misreading it as absence would be the same defect.

**State 14 is defensive on the same footing, and an earlier draft of this
amendment called it a live legacy observation.** That description does not
survive the version history it appeals to. The pre-Fork-1 daemon declares
protocol version 2 and tests protocol mismatch BEFORE the namespace check;
ADR-096 Fork 1 bumped the version to 3 and removed the namespace reject
outright; this build probes at version 4. So the only peer that still sets
`namespace_mismatch` is one that answers a version-4 probe by reaching its
protocol-mismatch branch first, which produces **state 2**. There is no
conforming version at which a real peer produces state 14. It is retained
exactly as state 4 is — because a frame that sets the flag is unambiguous proof
of a live process whatever produced it, and reading it as absence would be the
same defect — and its no-unlink, no-replace disposition stands unchanged, which
is the whole reason retaining it costs nothing.

Two consequences of the existing contract are worth stating because they are
easy to invert:

- **State 3 is not automatic replacement.** The Consequences section's
  description of the daemon as disposable, in the sense that killing and
  respawning it is safe, licenses replacement being _safe_. It does not make
  replacement something boot performs unprompted. Disposable does not mean
  auto-replaced at boot.
- **State 12 is a clean exit, and it is where the class rule came from.** The
  convergence requirement already anticipates multiple launch attempts and lets
  the daemon-side fence choose the sole owner, so losing that fence is
  legitimate rather than an error. Blocking and re-probing belong to the
  client-side recoverer, never to daemon boot.

  State 12 is transient — a peer between fork and bind — and an earlier draft of
  this amendment classed refusals by transience, which put state 12 in the
  retryable class and exited nonzero. It was wrong, and working out why is what
  produced the rule now stated above: the resolver of state 12 is the _other_
  process, which is already booting and will own the rendezvous. Restarting this
  one cannot help, and a restart loop is the likely result. Exit 0 encodes
  "someone else has this", not "nothing is wrong".

  **The refusal must name the pid AND its command line**, because the class rule
  has one failure mode and this is it. A pid file can outlive its writer and the
  number be reused by an unrelated long-lived process. That reads as state 12
  forever: the pid is live, so it is never cleaned; the disposition is exit 0, so
  a supervisor configured to restart only on unsuccessful exit never retries. The
  boot is wedged, permanently, and correctly according to every rule above —
  because the one thing the rules cannot check is whether the live pid is a
  khived at all. Printing the command line beside the pid is what lets an
  operator see in one line that the incumbent is not a khived, which is the only
  evidence that distinguishes a genuine race from a stale rendezvous with a
  recycled pid. A refusal that prints the bare number leaves them with nothing to
  act on and no signal that anything is wrong.

  **The same-process case has its own disposition.** When the recorded pid is
  this process and same-process incumbency is permitted, boot has not lost the
  fence to anyone — it IS the incumbent — so the disposition is `MayBind` and
  there is no exit at all. That is the only branch in the whole table where a
  live pid yields `MayBind`, and it is licensed solely by the pid being this
  process. A harness must assert the pid identity, not merely that binding
  succeeded, or it cannot tell this branch from a collapse into cleanup.

### Replacing a live incumbent

This section specifies a capability that **does not exist yet**. No replacement
flag is present on any current command surface, and nothing here describes
maintained behaviour. It is written as a contract so that the capability, when
built, is built with the safety sequence rather than acquiring it afterwards.
Implementing it requires naming the owning subcommand and its parsing, which
this amendment deliberately leaves to the implementing change.

Removal of a live incumbent is operator-elected and gated behind an explicit
`--replace-incumbent` opt-in. Every step is required:

1. Classify. Proceed only from states 1 through 4, where a daemon identity was
   positively read. Never from states 5 through 9 or 12 through 14 — those have
   either not established what would be killed, or not established that anything
   is there at all. State 5 in particular is excluded precisely because an
   identity that could not be resolved is not an identity that was refuted.

   **State 14 is deliberately NOT on this list, and an earlier draft of this
   amendment had it there.** The reasoning that put it there was that a daemon
   serving a different namespace is an incumbent like any other. That reasoning
   is obsolete: ADR-096 Fork 1 removed the namespace reject outright, so a
   correctly built daemon serves a differently-namespaced frame rather than
   refusing it, and `namespace_mismatch` is never set on any serve path
   (`crates/khive-mcp/src/daemon.rs`, the ADR-096 Fork 1 test asserts
   `!resp_other.namespace_mismatch` with the message "ADR-096 Fork 1 removed the
   namespace_mismatch reject"; no serve-side site sets the field true, while the
   sibling `config_mismatch` and `version_mismatch` fields both have such
   sites). State 14 therefore survives only as a defensive classification, not
   as a live legacy observation: a peer old enough to still set the flag is also
   old enough to fail this build's version check first, so it answers with
   `version_mismatch` and lands in state 2 instead. The one
   thing that must never be done on the word of an obsolete signal is to kill
   the process that sent it. Refuse, name both namespaces, leave the rendezvous
   alone.
2. **Bind the pid to the socket, on evidence the incumbent cannot author.** The
   classification above proves that _something_ live is serving the socket and
   what protocol and configuration it speaks. It does not prove that the process
   named by the pid file is that something. A pid file can be stale or its pid
   reused while an unrelated process answers on the socket, and signalling on
   that evidence kills a process this boot never contacted.

   **An earlier draft of this amendment required the acknowledgement to carry
   the serving process's own pid (`served_pid`) and justified that as the
   portable choice. It was wrong twice over.** It was circular: the threat this
   step exists to answer is an unrelated process answering on the socket, and
   the remedy asked exactly that process to name itself — a same-uid squatter
   supplies the recorded integer and passes. And its portability premise was
   false in this repository, which already performs the platform-specific
   peer-credential lookup the draft called unavailable: `peer_uid` reads
   `getpeereid(2)` on macOS/BSD and `SO_PEERCRED` on Linux
   (`crates/khive-runtime/src/daemon.rs`), is used on the accept path, and
   carries a regression test asserting it reports the connecting process's uid.

   The binding evidence is therefore the **kernel's answer for the peer of the
   probe connection**, never a field the responder fills in: the peer pid from
   `SO_PEERCRED` on Linux or `LOCAL_PEERPID` on macOS, required to equal the
   recorded pid. Where the platform exposes no peer pid, **replacement is
   unavailable on that platform** — refuse and say so. Where the lookup fails,
   or the peer pid does not equal the recorded pid, replacement is likewise
   unavailable — refuse and name both values. Do not fall back to signalling the
   recorded pid, and do not fall back to a responder-supplied value. An
   ownership check that degrades to "signal it anyway" is not a check, and one
   the suspect can answer about itself is not evidence.
3. Signal `SIGTERM` to the pid established in step 2.
4. Wait, bounded, for process death. Death is **observed**, never assumed.
5. On timeout, refuse: leave the socket intact and name the pid. Never escalate
   to `SIGKILL` implicitly, and never unlink a socket whose owner is still
   alive.
6. Only after observed death, remove the socket and pid file, then bind.

Steps 2 and 4 are the substance. Step 4 because removing a rendezvous file is
not a way to stop a process; step 2 because every other step is careful about
_how_ the incumbent is stopped while assuming _which_ process it is.

Neither the peer-pid lookup nor any equivalent exists on the probe path today:
`peer_uid` establishes only the peer's uid, and `LOCAL_PEERPID` is not spelled
anywhere in this repository. Adding a peer-pid lookup beside `peer_uid`, and
carrying its result out to the classification, is part of implementing this
section; until it exists `--replace-incumbent` cannot be built to this contract.
`served_pid` is named above only to record what was rejected, and must not be
added to the response frame.

### Test obligations

One test per state, each asserting the **outcome** rather than the return value.
For every refuse state that must assert: the incumbent is still alive, the
socket still exists, and no second listener was bound. A test that checks only
the returned error passes while the socket is unlinked underneath a live
process, which is precisely the failure being prevented. Each refuse state also
asserts its exit code, since an otherwise-correct refusal carrying the wrong
code either drives a supervisor restart loop or silently retires a retryable
state.

"One test per state" is not by itself enough to tell a conforming test from one
that checks an error value, so each state's test declares four things
explicitly. Without all four, a test can be green while proving nothing:

| Element                      | What it fixes                                                                                                                                                                            |
| ---------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Peer setup**               | Exactly what the fixture puts behind the socket: a real daemon, a synthetic listener with a scripted reply, or nothing. Names which, and for a scripted reply, the exact response frame. |
| **Probe input**              | The frame boot sends, so a test cannot accidentally exercise a different state than the one it names.                                                                                    |
| **Rendezvous postcondition** | Whether the socket file and pid file still exist afterwards, asserted by stat, not inferred from the absence of an error.                                                                |
| **Listener count**           | How many processes hold the socket after boot returns. This is the assertion that actually detects the double-bind, and no other element substitutes for it.                             |

**The four elements, instantiated per state.** Stating the requirement without
instantiating it leaves the test author to invent fixtures, and an invented
fixture is exactly how a test ends up green against the wrong branch. Every row
below is binding. `boot frame` means the standard probe: `probe_only` set, this
build's `protocol_version`, this build's `config_id`. "scripted listener" means
a socket bound by the test that replies with the frame given and nothing else.

**The frames below show only the fields that distinguish each case; they are not
complete wire frames, and a harness must not transmit them literally.**
`DaemonResponseFrame` declares `namespace_mismatch` with no `serde` default
(`crates/khive-runtime/src/daemon.rs`), unlike its `config_mismatch`,
`served_config_id`, `version_mismatch`, and `daemon_protocol_version` siblings.
A row transmitted as written therefore fails to deserialize at the client, and
rows 2, 3, 5, 6, and 14 would every one of them classify as state 8 — the exact
state they exist to be distinguished from. Each fixture serializes a **complete**
frame: the fields shown set as shown, every remaining field at its zero value.

| #   | Peer setup                                                                                                                  | Probe input | Rendezvous postcondition              | Listeners after                                         | Exit |
| --- | --------------------------------------------------------------------------------------------------------------------------- | ----------- | ------------------------------------- | ------------------------------------------------------- | ---- |
| 1   | Real daemon, same version, same `config_id`                                                                                 | boot frame  | socket + pid file unchanged           | 1 (incumbent)                                           | 0    |
| 2   | Scripted listener replying `{ok:false, version_mismatch:true, daemon_protocol_version:<other>}`                             | boot frame  | socket + pid file unchanged           | 1 (scripted)                                            | 0    |
| 3   | Scripted listener replying `{ok:false, config_mismatch:true, served_config_id:<other>}`                                     | boot frame  | socket + pid file unchanged           | 1 (scripted)                                            | 0    |
| 4   | Scripted listener replying with `metrics` populated. **Synthetic**: the real boot frame cannot elicit this                  | boot frame  | socket + pid file unchanged           | 1 (scripted)                                            | 0    |
| 5   | Scripted listener replying `{ok:true}` with `served_config_id` ABSENT and no mismatch flag                                  | boot frame  | socket + pid file unchanged           | 1 (scripted)                                            | 6    |
| 6   | Scripted listener replying `{ok:true, result:<any JSON>}` — ack-shape test fails on `result`                                | boot frame  | socket + pid file unchanged           | 1 (scripted)                                            | 0    |
| 7   | Scripted listener that accepts the connection and writes nothing until past the probe deadline                              | boot frame  | socket + pid file unchanged           | 1 (scripted)                                            | 2    |
| 8   | Scripted listener that writes bytes which are not a valid frame                                                             | boot frame  | socket + pid file unchanged           | 1 (scripted)                                            | 3    |
| 9   | Socket file present with nothing bound to it; pid file names a live process (a sleeper is fine)                             | boot frame  | socket + pid file unchanged           | 0                                                       | 4    |
| 10  | Socket file present with nothing bound; pid file names a dead pid, **and a second case with the pid file removed entirely** | boot frame  | socket + pid file REPLACED            | 1 (this process)                                        | —    |
| 11  | No socket file; pid file names a dead pid or is absent                                                                      | boot frame  | socket + pid file created             | 1 (this process)                                        | —    |
| 12  | No socket file; pid file names a live process that is NOT this one                                                          | boot frame  | pid file unchanged, no socket created | 1 (that process)                                        | 0    |
| 12a | No socket file; pid file names THIS process, same-process incumbency permitted                                              | boot frame  | socket created by this process        | 1 (this process)                                        | —    |
| 13  | Socket path present but not connectable for a reason other than refusal (e.g. mode 000 → `EACCES`)                          | boot frame  | socket + pid file unchanged           | not observable — assert the postcondition and exit only | 0    |
| 14  | Scripted listener replying `{ok:false, namespace_mismatch:true}`                                                            | boot frame  | socket + pid file unchanged           | 1 (scripted)                                            | 0    |

Row 12a is the `MayBind` branch and it is the only row where a live pid ends in
binding. Its test asserts that the pid it matched is this process's own, not
merely that a bind succeeded: without that assertion the row is
indistinguishable from a collapse into cleanup, which is the defect the whole
table exists to prevent.

Row 13's listener count is stated as not observable rather than given a number,
because a socket that cannot be connected to also cannot be interrogated for who
holds it. Writing a number there would be a fabricated assertion.

Two states need their mechanism named because the general recipe does not reach
them. **State 4** cannot be elicited by the real boot probe, so its test is
explicitly synthetic: a scripted listener that returns a metrics-carrying reply.
The test asserts the classification, and must be labelled synthetic so a later
reader does not mistake it for evidence that boot can reach the state. **States
10 and 11** are the two that bind, so their postcondition is inverted: the test
asserts the rendezvous was replaced and that exactly one listener — this process
— holds it afterwards.

Where a state's test runs below process level, the exit code is asserted against
the value the classification maps to rather than against a real process exit,
and at least one end-to-end test per exit class asserts a real process exit code
so the mapping itself is covered.

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
