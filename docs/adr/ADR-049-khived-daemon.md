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
none of it should be read as an account of what boot does today. Bringing the
code to this contract is a prerequisite for the guarantees stated here, not a
consequence of recording them.

What boot does today is further from this contract than a collapsed
classification. `cleanup_stale_daemon` in `crates/khive-runtime/src/daemon.rs`
sends no probe at all: it reads the pid file, and keeps the rendezvous only if
the pid parses, names a process that is running, the socket path exists, and a
plain connect to it succeeds. Any other outcome unlinks both the socket and the
pid file. So there is no reply to classify on this path, and the states below
have no counterpart in it.

That matters in a specific direction. Because the predicate is conjunctive and
begins at the pid file, a **live** daemon whose pid file is missing, truncated,
unreadable, or holding a pid the check declines to accept falls through to the
unlink branch and has its socket removed underneath it, purely on the strength
of a file that is not the thing being tested for liveness. In the other
direction, once all four conjuncts hold the rendezvous is kept whatever the
peer would have replied, because nothing asks it. The amendment's
classification replaces this predicate; it does not refine it.

A probe that collapses the distinctions this amendment draws does exist, but it
answers a different question and it is not itself a boolean.
`probe_daemon_identity` in `crates/khive-mcp/src/daemon.rs` returns a four-way
`ProbeOutcome` — `Alive`, `Dead`, `Timeout`, and a lock-contended variant — so
its callers can already separate a slow peer, and an unconfirmable peer boot,
from a dead or mismatched one. What is collapsed is narrower: reaching `Alive`
requires one boolean, `is_probe_ack`, conjoined with three mismatch flags,
protocol version, and served config id, and everything failing that conjunction
lands in the single `Dead` bucket. Its callers ask whether a _client_ may
dispatch to the daemon or must fall back — including its own boot fence, which
is not the daemon-boot rendezvous decision governed here.

Its ack test is also weaker than the definition below, and deliberately so for
its own purpose: `is_probe_ack` is `ok && result.is_none() && error.is_none()`,
which does **not** exclude a reply carrying `metrics` or a `request_id`. An
implementer must not read that predicate as an implementation of Amendment 6's
ack shape; under this amendment a metrics-bearing reply is state 4 and never an
acknowledgement. The function is the closest existing analogue to the
classification below and a useful reference, but it is not the predicate this
amendment changes, and its ack sentinel is not the one this amendment defines.

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
retry of this process: an operator changing configuration, or another process
resolving the rendezvous. Naming a possible resolver is not a finding that one
exists. The code says only that the resolution lies outside this process. A
refusal exits nonzero when the resolver IS a later retry of this process, so
that under a supervisor configured to restart on nonzero a restart is the
remedy. The codes are chosen for that policy; a supervisor configured otherwise
reads them differently, and the choice is stated as a choice in Amendment 7.

Transience is not the test. A rollout in which an older daemon answers the probe
can last hours and still exits 0, because no number of restarts of this process
ends it. Conversely a reply that failed to parse may be a one-off and exits
nonzero, because retrying is exactly what might succeed. How long a condition
lasts and who resolves it are independent properties, and only the second one is
the test.

An earlier draft made that point with a peer caught between fork and bind,
offered as the short-lived case that still exits 0. That example is withdrawn:
the code publishes the socket before the pid file, so no observation of a
booting peer produces state 12 at all. The open note below records the reading
at source.

**What "who resolves it" asks, stated once.** Nonzero asks whether a later
attempt of this process can observe a _different classification_, not whether
this attempt could have bound. That is the reading states 5 and 8 already use: a
peer that has not published its identity may publish it before the next probe,
and a reply that failed to parse may parse next time, so in both the retry is
the resolver. State 15 is the counter-case — an identity the peer positively
published cannot be re-observed differently, so no retry reaches a different
classification and it exits 0. State 7 is nonzero on the state 8 reading: a peer
that did not answer within the deadline may answer within the next one.

**The split between exit 4 and exit 0 for states 9 and 12 was unlicensed. It is
closed by Amendment 7, which gives both states exit 0 and retires exit 4.** The
note below is kept as written because it is the evidence Amendment 7 rests on,
and because the premise it withdrew must not be reopened. Asking only whether a
later attempt observes a different classification is necessary but not
sufficient, because it is true of both states. They arise from the same instant
of the same race and only the socket separates them, so the class rule alone
does not say which of them gets a retryable code.

An earlier version of this section supplied that license with a trajectory
argument: a socket present with a live refusing owner was said to be a daemon
_past_ its listener, and a live recorded pid with no socket a peer _before_ its
listener, whose next state is ownership. **That argument is withdrawn.** Three
facts, each read at source, are why. They are recorded here so the question is
not reopened from the same premise:

1. **Boot publishes the socket before the pid file.** In
   `crates/khive-runtime/src/daemon.rs`, `UnixListener::bind(&sock)` is at line
   1644 and `write_pid_file_exclusive(&pid_file)` at line 1662, with only the
   chmod and the socket-identity capture between them. A booting peer therefore
   has a socket before it has a pid record, so "no socket, live recorded pid"
   cannot be a peer caught between fork and bind: the pid file does not exist
   yet at that point in boot.

2. **Shutdown runs the other way, but a conforming boot cannot observe it.** The
   lifecycle rule earlier in this document has SIGTERM/SIGINT stop accepting,
   drain in-flight requests, then remove socket and PID file, in that order. An
   earlier version of this note inferred from that ordering that a state 12
   observation is at least as consistent with a daemon about to exit as with one
   about to bind, and that the reading therefore argues for nonzero. **That
   inference does not survive the locking, and is withdrawn.** Shutdown unlinks
   only while holding the recovery lock, and when it cannot acquire that lock it
   skips the unlink entirely rather than proceeding unlocked
   (`crates/khive-runtime/src/daemon.rs:1856-1870`); the two unlinks themselves
   are adjacent syscalls inside that critical section (`:1888-1892`). Daemon boot
   must hold the same exclusive lock across cleanup, bind and pid-write
   (`daemon.rs:186-195` and `:268-281`). A conforming competing boot therefore
   blocks until both files are gone, and cannot observe the orderly
   socket-gone/pid-live interval at all. The shutdown ordering stands as a fact.
   It licenses nothing about state 12, in either direction.

3. **A live recorded pid proves nothing about ownership, in either state.**
   Liveness is `kill(pid, 0)` (`daemon.rs:1943-1996`) and the incumbent check is
   a plain connect. Neither establishes that the recorded pid owns the socket,
   or that it is a khived at all. So a state 9 observation does not license
   "past its listener" either: daemon A can bind, write pid p, crash before
   cleanup, leave a socket that refuses connections, and the OS can reuse p for
   an unrelated live process. This document already says elsewhere that a live
   pid may belong to an unrelated long-lived process; the withdrawn clause
   forgot it.

What is left, once the boot window is impossible and the shutdown window is
unobservable, is fact 3, and fact 3 is a statement about what cannot be known
rather than about what is happening. Ownership and trajectory are both unproven
in either state. Pid reuse is one explanation the observation admits and the one
that no retry of this process ever resolves, but the facts do not establish that
it is the explanation in any given case, and nothing here should be read as
saying they do. Neither state licenses a trajectory reading, then, and these
exit codes are normative for supervisors. Resolving the split was deliberately
out of scope for Amendment 6: the classification was already the improvement
over the code, and inventing a second mechanism to replace a refuted one, in the
round that refuted it, is how the first one got here. **Amendment 7 resolves it
on the ground stated there, and the table below carries the resolved codes.**

**Codes are numeric and distinct, not merely "nonzero".** A supervisor and an
end-to-end test both need to tell these apart, and "nonzero" is a class, not a
value:

| Exit | Meaning                                                            | States                           |
| ---- | ------------------------------------------------------------------ | -------------------------------- |
| 0    | Refused; resolver is an operator or another process                | 1, 2, 3, 4, 6, 9, 12, 13, 14, 15 |
| 1    | Reserved for unclassified failure; never emitted by classification | —                                |
| 2    | Refused; connected but the peer did not answer                     | 7                                |
| 3    | Refused; the peer's reply did not parse                            | 8                                |
| 4    | Reserved; formerly state 9, retired by Amendment 7                 | —                                |
| 5    | Reserved; formerly state 13, retired by Amendment 6                | —                                |
| 6    | Refused; the peer answered but its identity could not be resolved  | 5                                |

Exit 1 is reserved so that an unclassified crash can never be mistaken for a
classified refusal. States 10, 11, and 12s have no exit code because they
proceed to bind rather than refusing. 12s is the only one of the three that
binds with a live recorded pid, and it is licensed solely by that pid being
this process: a harness must assert the pid identity, not merely that binding
succeeded, or it cannot tell this branch from a collapse into cleanup.

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

| #   | State                                             | Observable                                                                                                                                                           | Disposition                                                                                                                                                                                            | Exit |
| --- | ------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ---- |
| 1   | Exact acknowledgement                             | connect ok, probe ack, identity matches                                                                                                                              | Refuse to start; report the incumbent pid                                                                                                                                                              | 0    |
| 2   | Protocol-version mismatch                         | connect ok, and either `version_mismatch` set OR `daemon_protocol_version` unequal to this build's — the two are independent fields, see the precedence rules below  | Refuse; name both versions and the remedy. Never unlink                                                                                                                                                | 0    |
| 3   | Config mismatch                                   | connect ok, `config_mismatch` set — `served_config_id` may be absent, and is not part of this predicate                                                              | Refuse by default; name the first differing field without values when `served_config_id` is present, otherwise say identity was not echoed. Never unlink; replacing the incumbent is out of scope here | 0    |
| 4   | Metrics-only reply                                | connect ok, reply carries metrics                                                                                                                                    | Refuse to start                                                                                                                                                                                        | 0    |
| 5   | Identity unresolved                               | connect ok, reply HAS ack shape, no mismatch flag set, `served_config_id` ABSENT                                                                                     | Refuse to start; name the pid and that identity could not be established                                                                                                                               | 6    |
| 6   | Parseable non-acknowledgement                     | connect ok, frame deserializes, reply does NOT have ack shape                                                                                                        | Refuse to start; name the observed shape                                                                                                                                                               | 0    |
| 7   | Silent connect                                    | connect ok, no parseable response within the deadline                                                                                                                | Refuse; distinct error "connected but did not answer"                                                                                                                                                  | 2    |
| 8   | Malformed reply                                   | connect ok, bytes returned, frame does not parse                                                                                                                     | Refuse to start                                                                                                                                                                                        | 3    |
| 9   | Connection refused, pid live                      | `ECONNREFUSED`, pid running                                                                                                                                          | Refuse; name the pid AND its command line                                                                                                                                                              | 0    |
| 10  | Connection refused, no live listener              | `ECONNREFUSED`, and the recorded pid is not running OR no usable pid record exists (file absent, empty, or not parseable as a pid)                                   | Clean up and bind                                                                                                                                                                                      | —    |
| 11  | No socket, no live pid                            | socket absent, and the recorded pid is not running OR no usable pid record exists (file absent, empty, or not parseable as a pid) — the same predicate state 10 uses | Clean up and bind                                                                                                                                                                                      | —    |
| 12  | No socket, pid live, incumbency not permitted     | socket absent, pid running, and EITHER the pid is not this process OR same-process incumbency is not permitted                                                       | Refuse; name the pid AND its command line                                                                                                                                                              | 0    |
| 12s | No socket, pid live, THIS process                 | socket absent, pid running, the pid IS this process, and same-process incumbency is permitted                                                                        | `MayBind` — boot has not lost the fence to anyone, so it proceeds to bind                                                                                                                              | —    |
| 13  | Other connect error (`EACCES`, `ENOTSOCK`, …)     | connect fails, not refused                                                                                                                                           | Refuse; name the errno. Never retry, kill, or spawn (Amendment 4)                                                                                                                                      | 0    |
| 14  | Namespace mismatch (**defensive**, not reachable) | connect ok, `namespace_mismatch` — no conforming daemon sets this since ADR-096 Fork 1, and a peer old enough to set it fails the version check first                | Refuse; name both namespaces. Never unlink, never replace                                                                                                                                              | 0    |
| 15  | Identity positively unequal                       | connect ok, reply HAS ack shape, no mismatch flag set, `served_config_id` PRESENT and unequal to this build's                                                        | Refuse to start; name the pid and both `config_id` values                                                                                                                                              | 0    |

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

1. No socket → state 11 if there is no live recorded pid; otherwise state 12s
   if that pid is this process and same-process incumbency is permitted, else
   state 12. That `else` is exhaustive by construction, and state 12's predicate
   admits everything it carries: the foreign-pid case, and the same-process case
   where incumbency is not permitted.
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
8. Frame deserializes, has ack shape, no mismatch flag, `served_config_id`
   ABSENT → state 5.
9. Frame deserializes, has ack shape, no mismatch flag, `served_config_id`
   PRESENT and unequal → state 15. This is the same distinction the protocol
   version already draws two steps above: an identity the peer positively
   published and that does not match is refuted, not unresolved, so no retry of
   this process can change it and it exits 0.
10. Frame deserializes, does not have ack shape → state 6. This is the catch-all
    and it is evaluated LAST among response rows. It must never fall through to
    cleanup.

States 5 and 15 are both evaluated before the state 6 catch-all deliberately.
Under a naive first-match-by-row-number rule the catch-all would absorb every
one of their replies, because a reply whose `served_config_id` is missing or
wrong can be described as "not an acknowledgement" — and a conforming
implementation could then never produce the identity diagnosis the table
promises.

**States 5, 6, and 15 are the ones an enumeration drawn from a single build will
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
but carries NO `served_config_id`, while no mismatch flag is set. Identity that
cannot be established is not identity that was refuted, and neither is death. It
exits nonzero rather than 0 because a peer that has not yet published its
identity may publish it before the next attempt, which makes a retry of this
process the resolver.

State 15 is the other half of that observation and it exits 0, because the class
rule turns on who resolves the condition. A peer that published a
`served_config_id` and published a different one has refuted this build's
identity positively. No number of restarts of this process changes what the
incumbent echoes, so the resolver is an operator or the other process. An
earlier draft folded both halves into state 5 and gave the pair exit 6, which
under the restart-on-nonzero policy tells a supervisor to restart against a
condition a restart cannot reach. The
reasoning is exactly the one already applied to an unequal
`daemon_protocol_version` two steps earlier in the precedence: a positively
refuted identity is not an unresolved one.

Every state above is reachable from the boot probe except **states 4, 14, and
15**.
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

**State 15 is defensive for a third reason: a conforming daemon that sees a
different `config_id` sets `config_mismatch`, which is state 3.** So a peer that
publishes an unequal `served_config_id` with no flag set is either
non-conforming or a future variant that reports identity without judging it.
The state exists because the observation is unambiguous — the peer answered, and
it is not us — and because the alternative is to route it into state 5, which
would attach a retryable exit to a condition no retry of this process can
change. Retaining it costs nothing for the same reason states 4 and 14 do: its
disposition refuses and touches nothing.

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

  An earlier draft of this amendment classed refusals by transience, which put
  state 12 in the retryable class and exited nonzero. It was wrong, and working
  out why is what produced the rule now stated above: where the resolver is the
  _other_ process, restarting this one cannot help, and under the
  restart-on-nonzero policy Amendment 7 selects, a restart loop is the likely
  result. Exit 0 encodes "the resolution lies outside this process", not
  "nothing is wrong" and not "someone else has this".
  That same draft called state 12 transient, a peer between fork and bind. That
  description is withdrawn for the reason recorded in the note above, and the
  exit code rests on the asymmetry Amendment 7 states, not on that mechanism.

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

  **The same-process case splits on whether incumbency is permitted, and both
  halves are classified.** When the recorded pid is this process and
  same-process incumbency IS permitted, boot has not lost the fence to anyone —
  it IS the incumbent — so the disposition is `MayBind`, there is no exit at
  all, and that is state 12s. An earlier draft wrote that as a conditional
  clause inside state 12's disposition, which left an implementer following the
  exit enumeration with no named outcome for it; it is its own row now, and the
  precedence names it.

  When same-process incumbency is NOT permitted, the observation is state 12 and
  refuses. **That is the production path**, not a corner: `run_daemon` is called
  with `allow_same_process_incumbent = false`
  (`crates/khive-runtime/src/daemon.rs:1327`; the `true` path at `:1348` is the
  in-process test entry). A previous version of state 12's predicate required
  the pid NOT be this process, which left that production-reachable observation
  routed by precedence into a state whose own predicate it failed, with no
  disposition and no exit code covering it — an unclassified observation inside
  a table whose whole claim is that classification is closed. State 12's
  predicate now admits it, and the disposition needs no special case: naming the
  pid and its command line prints this process's own line, which is exactly the
  tell an operator needs to see that boot refused itself rather than a stranger.

  12s remains the only state in which a live recorded pid yields `MayBind`,
  licensed solely by that pid being this process with incumbency permitted.

### Replacing a live incumbent — out of scope

Replacement of a live incumbent is out of scope for this amendment and will be
specified in its own ADR when the capability is built. Nothing in this amendment
authorises signalling a process. That ADR inherits three named open items, all
of which surfaced in review of this one and should not have to be rediscovered:

- **Ownership evidence.** A destructive action needs a kernel-authenticated peer
  pid. A self-reported pid on the response frame is not resistant to a same-uid
  peer that lies, so it cannot on its own license signalling.
- **The step-6 unlink race.** Removing the socket and pid file after observed
  death can delete a rendezvous a _new_ owner created in the interval. The
  existing recovery guard at `crates/khive-mcp/src/daemon.rs:1038-1075` rechecks
  the pid file and a live socket before unlinking; a replacement sequence needs
  an equivalent recheck, plus a lock held through cleanup and bind.
- **Platform reach.** A kernel peer-pid primitive is not available everywhere
  this daemon compiles, so "both platforms expose such a lookup" is not a claim
  the tree supports. `peer_uid` at `crates/khive-runtime/src/daemon.rs:319-375`
  is `#[cfg(unix)]` with three arms: `getpeereid` on Apple targets, `SO_PEERCRED`
  on Linux, and a third arm returning `ErrorKind::Unsupported` for every other
  Unix. The new ADR must say what replacement does on that third arm rather than
  degrading to a self-reported value.

### Test obligations

One test per state, each asserting the **outcome** rather than the return value.
For every refuse state that must assert: the incumbent is still alive, the
socket still exists, and no second listener was bound. A test that checks only
the returned error passes while the socket is unlinked underneath a live
process, which is precisely the failure being prevented. Each refuse state also
asserts its exit code, since under the restart-on-nonzero policy these codes are
chosen for, an otherwise-correct refusal carrying the wrong code either drives a
restart loop or silently retires a retryable state.

Where a state's test runs below process level, the exit code is asserted against
the value the classification maps to rather than against a real process exit,
and at least one end-to-end test per **emitted** exit class asserts a real
process exit code so the mapping itself is covered. The emitted classes are 0,
2, 3, and 6. Codes 1, 4, and 5 are reserved and unemitted by classification, so
they are deliberately not exercised as classification outcomes; requiring an
end-to-end test for them would require producing an outcome this document
forbids. A test that asserts no classified refusal ever exits 1, 4, or 5 is
welcome but belongs to the reserved-code guarantee, not to this per-class
obligation.

"One test per state" is not by itself enough to tell a conforming test from one
that checks an error value, so each state's test declares four things
explicitly. Without all four, a test can be green while proving nothing:

| Element                      | What it fixes                                                                                                                                                                            |
| ---------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Peer setup**               | Exactly what the fixture puts behind the socket: a real daemon, a synthetic listener with a scripted reply, or nothing. Names which, and for a scripted reply, the exact response frame. |
| **Probe input**              | The frame boot sends, so a test cannot accidentally exercise a different state than the one it names.                                                                                    |
| **Rendezvous postcondition** | Whether the socket file and pid file still exist afterwards, asserted by stat, not inferred from the absence of an error.                                                                |
| **Listener count**           | How many processes hold the socket after boot returns. This is the assertion that actually detects the double-bind, and no other element substitutes for it.                             |

**The per-state fixture recipe is specified separately.** Instantiating the four
elements for every state — the exact peer setup, the exact response frame, and
the expected listener count per row — is a second normative surface with its own
failure modes, and it is where the review of this amendment concentrated its
findings. It belongs in a follow-up ADR rather than here, so that the
classification contract above can be read and implemented on its own.

Two constraints that follow-up inherits, because both were established while
writing this one and should not be rediscovered:

- A fixture must serialize a frame that both deserializes at the client AND
  passes the version check. `DaemonResponseFrame` declares `namespace_mismatch`
  with no `serde` default (`crates/khive-runtime/src/daemon.rs`), unlike its
  `config_mismatch`, `served_config_id`, `version_mismatch`, and
  `daemon_protocol_version` siblings, so an abbreviated frame fails to
  deserialize and lands in state 8. But "complete frame, every unspecified field
  at its zero value" is equally wrong: `daemon_protocol_version` zero is unequal
  to this build's, and precedence puts version mismatch ahead of the identity and
  ack-shape rows, so such a frame lands in state 2 whatever else it sets. The
  recipe therefore has to define an explicit valid baseline frame and override
  only the distinguishing fields per row.
- State 13 is not observable at the listener-count element: the socket path is
  present but not connectable, so no fixture can count who holds it. It is
  exempt from that one element, and the substitute is fixed here rather than
  left to the recipe, because an open-ended "testable substitute" readmits
  exactly the test this section excludes everywhere else — one that asserts the
  returned error and nothing about what boot did.

  The substitute is a seam recording boot's **attempts**: how many times it
  tried to bind the socket, and how many times it tried to unlink either
  rendezvous path. A conforming state-13 test asserts both counts are zero and
  that the socket and pid paths are unchanged by stat, in addition to the exit
  code. Attempt counts rather than outcomes, because the failure being excluded
  is boot _trying_ and being defeated by the environment, which is
  indistinguishable from boot correctly refusing if only the end state is read.
  The connect error alone is never sufficient: it describes what boot saw, not
  what boot then did.

  Never waive the element that detects the double-bind while still calling it
  mandatory.

Two obligations belong to this classification contract rather than to the
fixture recipe, and both are required here because their absence is what allowed
the collapse this amendment corrects:

- A test that starts a real live incumbent and drives each non-acknowledgement
  class against it, asserting that startup never leaves two live daemons.

Each state's guard carries a mutation control: defeat the guard, confirm that
exactly that state's test fails, and restore from a snapshot rather than by
reapplying an inverse edit.

### What is unchanged

The convergence requirement, the client-side recoverer lock, the daemon-side
boot fence, the exactly-once recovery boundary of Amendment 3, and the socket
accessibility narrowing of Amendment 4 all stand as written. This amendment
constrains only what boot may conclude from a probe result, and what it may do
about it.

## Amendment 7 (2026-08-27): states 9 and 12 share one exit code

Amendment 6 classified the incumbent states and left one question open: why
state 9 exits 4 and state 12 exits 0, when its own reading says ownership and
trajectory are unproven in both. This amendment closes it. **State 9 exits 0.
Exit 4 is retired and not reused. State 9's refusal names the pid and its
command line, as state 12's already does.**

### Why neither code was licensed by evidence

The class rule asks whether a later attempt of this process can observe a
different classification. Amendment 6 established that the answer is the same in
both states, so the rule does not separate them. Nothing else did either.

**What exit 0 means here, stated so it cannot be read as an ownership claim.** A
nonzero code says a later attempt of this process is the remedy. Exit 0 says the
opposite: this process must not retry, and whatever resolves the condition lies
outside it. That is the exit table's own reading, "resolver is an operator or
another process", and it is a statement about where the resolution lives, not a
finding that some particular other process holds the rendezvous. Amendment 6's
fact 3 forbids the stronger reading in both states, and nothing below relies on
it.

State 9 licenses neither code on evidence. `ECONNREFUSED` with a live recorded
pid is consistent with a crashed daemon whose socket file outlived it and whose
pid was recycled by an unrelated process, and that is exactly the reading
Amendment 6 arrived at.

So the split cannot be settled on evidence, and pretending otherwise is what
produced the withdrawn trajectory argument. It is settled on the asymmetry of
being wrong.

### The asymmetry of harm, and the supervisor policy it assumes

**The asymmetry is not deployment-independent, and this document chooses a
policy rather than discovering a fact.** It optimises for a supervisor that
restarts on nonzero exit and treats exit 0 as a deliberate stop. That is the
shape `launchd` gives with `KeepAlive { SuccessfulExit: false }`, and it is what
the deployments this ADR is written for run. A supervisor configured the other
way — restarting on exit 0, or on both statuses, or alerting louder on nonzero —
inverts the reasoning below, and an operator running one should read this
section as stating which policy the codes were chosen against rather than as an
argument that holds everywhere.

Under that policy, a wrong exit 0 wedges boot **visibly**. The refusal is
terminal, so it happens once, an operator sees one refusal naming a pid and a
command line, and the command line is the evidence that tells them whether the
incumbent is a khived at all.

A wrong nonzero wedges boot **silently and repeatedly**. The supervisor restarts,
the next probe reads the same stale socket and the same recycled pid, and the
loop produces no information it did not have on the first attempt. This is the
failure mode the class rule exists to prevent, stated in Amendment 6 for state
12: exit 0 encodes "do not retry this here", not "nothing is wrong".

The two errors are not symmetric in cost under that policy, and only one of them
is observable by the person who can fix it. That is the whole license for the
code, and it is stated as such rather than dressed as a fact about what the
daemon is doing.

### The command line requirement propagates to state 9

Amendment 6 required state 12's refusal to print the pid **and** its command
line, because the class rule has exactly one failure mode — a recycled pid that
reads as a live incumbent forever — and the command line is the only evidence
that distinguishes it from a genuine race. **That requirement follows from
Amendment 6's fact 3, which applies to both states, so state 9 was always owed
it.** It is added here because this is the amendment that revisits state 9, not
because retiring exit 4 created the need: a nonzero code was never a substitute
for saying what the incumbent is.

What the exit-code change does is raise the operational cost of omitting it.
Under the restart-on-nonzero policy selected above, exit 0 makes the refusal
terminal, so the message is the only channel the operator gets, and a refusal
printing the bare number leaves them with nothing to act on. Under a policy that
restarts on either status the message is no less necessary: a restart may
re-observe state 9, since the socket and the live pid can both be unchanged, but
it cannot by itself establish who owns the rendezvous. That is Amendment 6's
third fact and it is all that fact supports. Whether some restart eventually
observes a different state depends on what happens outside this process, which
is the point.

### Exit 4 is retired, not reused

Like exit 1 and exit 5, exit 4 stays defined and unassigned. Nothing emits it
now, so no supervisor reads a 4 for state 9. The point of reserving rather than
recycling is the opposite one: an existing exit-4 rule somewhere must not start
firing on a **new** meaning silently attached to that number. This is the same
guarantee Amendment 6 gave when state 13 left exit 5.

### What this does not claim

It does not claim that a state 9 observation means another process owns the
rendezvous. Exit 0 here means this process must not retry and the resolution
lies outside it, which is a disposition rather than a finding about who holds
what. It claims that a boot which cannot tell the difference should stop and say
so, rather than declare itself retryable at something a retry of this process
does not resolve. What a supervisor then does with that is the supervisor's
policy, which is why the code is chosen for a stated one. The
three facts recorded in Amendment 6 are unchanged and remain the reason no
trajectory reading is available here.

### Test obligations

The per-class end-to-end obligation now covers 0, 2, 3, and 6. State 9's test
asserts exit 0 and asserts that the refusal message contains both the pid and
the command line, since dropping either is the failure this amendment guards
and neither is visible in an exit code. The reserved-code guarantee gains exit
4: no classified refusal ever exits 1, 4, or 5.

### What is unchanged

Every other state's disposition and exit code, the class rule itself, the
precedence order, the three withdrawn-premise facts, and all of Amendments 1
through 5. This amendment changes one exit code, one disposition string, and the
reserved set.
