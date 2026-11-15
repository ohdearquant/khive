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
  Amended by Amendment 11 (2026-09-23): a demand-mode daemon also retires on
  quiescence; a persistent-mode daemon is unchanged.

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
  When first-signal shutdown starts, this process-lifetime daemon restores the default SIGTERM and SIGINT dispositions, so a second signal during drain or cleanup terminates with that signal's status and abandons unfinished drain and cleanup.

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
  Amended by Amendment 11 (2026-09-23): this no longer holds unconditionally for a
  demand-mode daemon, which pays cold start again after it retires; persistent mode
  is unaffected.
- No search blocks on a rebuild — first query is FTS-instant in either mode.
- Single binary; `make local` unchanged. Daemon is transparent and optional.
- Dispatch logic is shared, so the daemon can never drift from local behavior.

**Negative / risks**

- A long-lived process holding the warm index uses resident memory (~the index size) for the
  machine's session. Mitigated by idle-exit being a cheap future addition; for now the daemon
  exits on signal and is re-spawned on demand.
  Amended by Amendment 11 (2026-09-23): a demand-mode daemon now retires on
  quiescence instead of waiting only on signal; persistent mode keeps this behavior.
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

## Amendment 8 (2026-08-30): cancellation preserves admitted request outcomes

Amendment 5 made client cancellation close the warm-daemon stream so daemon
read scopes could observe peer loss. That is safe for read work, but wrong for
a composed request that has crossed the write boundary: independent operations
may already have committed, and closing the stream discards the only response
that distinguishes those successes from validation failures. The MCP wrapper
then reported a bare `request cancelled`, inviting a blind retry of
non-idempotent operations. This amendment supersedes Amendment 5's client-side
stream-close rule while retaining its daemon-shutdown and read-interrupt rules.

Every agent-facing MCP request now has one request-group correlation id before
dispatch. A caller-supplied `request_id` is preserved; otherwise the bridge
mints an opaque nonzero `u64`. The daemon echoes that value and every operation
audit row stores it. The id is an observability label, not an operation id and
not a cross-attempt idempotency key.

Cancellation has one boundary:

1. If the cancellation signal is already visible before daemon admission, the
   bridge starts neither daemon forwarding nor local dispatch and reports a
   not-dispatched cancellation.
2. Once the bridge spawns the owned forwarding task, cancellation does not drop
   or abort it. A live MCP handler waits for the daemon's real terminal response
   and returns its ordinary per-operation envelope, including `status=partial`
   and the actual successes and failures. Dropping the handler detaches from
   the forwarding task; the task still completes, and the correlated audit rows
   remain the terminal observation.
3. If the forward returns the pre-write `None` outcome after cancellation, the
   bridge refuses local fallback. Cancellation cannot turn an unattempted
   daemon request into fresh local side effects.

The spawned forwarding future owns its request frame and pack list. This
ownership is the mechanical guarantee: its lifetime no longer borrows the MCP
handler, and dropping the handler's `JoinHandle` cannot drop the socket
exchange. A panic in that forwarding task is classified as outcome-unknown and
non-retryable, with the request id included when available.

This amendment does not promise exactly-once execution across process crashes,
irrecoverable transport loss, or a separately submitted retry. Those remain
outcome-unknown. It closes the cooperative-cancellation seam where the bridge
is alive and capable of preserving the daemon result, and it makes a vanished
handler's admitted work correlatable in the audit substrate.

### Test obligations (cancellation and bridge request ids)

- A cancellation delivered while a delayed daemon forward is in flight returns
  that forward's real partial envelope, including the original per-op failure.
- Aborting the outer handler after the forward begins does not prevent the owned
  forwarding future from reaching completion.
- An omitted request id is replaced with a nonzero bridge id exactly once per
  admitted attempt; an explicit caller value is unchanged.

## Amendment 9 (2026-09-10): bounded reconnect and classified read replay

Amendment 10 supersedes this amendment's replay eligibility, retry count, and
replay-window rules. Its lifecycle, missing-socket reconnect, and terminal
framing/protocol failure rules remain in force.

This amendment chooses client-side reconnect rather than simultaneous live-socket
replacement. Binding a temporary socket and renaming it over the public path is
not sufficient under the current live-incumbent refusal, exclusive PID claim,
and boot serialization contract. It would require a separate ownership protocol
for overlapping daemon lifetimes. The socket path can still be absent or refused
during a stop/start; there is no atomic-publication or zero-failure guarantee.

### Shutdown and recovery ordering

When the accept loop stops, the daemon closes its listener before draining
admitted connections and tracked background work. An unaccepted kernel backlog
is not admitted work and must not accept new frames without a handler. Startup
preserves an eligible live incumbent PID even when its socket is absent or
refuses connections: a closed listener does not mean draining work has finished.
This check protects serving ownership, not bootstrap work performed before the
dispatcher reaches this check. A recycled live PID can conservatively defer
direct startup; client recovery retains its separate process-identity check.

Recovery retains its recoverer-only lock across confirmation, signaling, exit
waiting, and the launch decision. It signals the observed incumbent under the
boot lock, releases that lock during bounded exit waiting so incumbent shutdown
can acquire it, then reacquires it and re-probes before cleanup or launch. A
changed PID or newly answering listener suppresses both unlink and launch.
The predecessor's shutdown still requires matching PID and socket device/inode
before unlinking; a same-directory rename by a successor therefore cannot make
old cleanup remove the successor's socket.

### Retry boundary

For `ENOENT` or `ECONNREFUSED` with a recorded live PID, clients poll at 100 ms
intervals for at most ten seconds, bounded further by the caller's original
request deadline. These attempts do not initiate lifecycle recovery. If the
window ends while the caller remains active, the existing recovery path decides
whether the daemon is genuinely absent. An expired or cancelled caller does not
start fresh recovery. Policy/access errors remain terminal, as in Amendment 4.

The MCP bridge additionally classifies the entire parsed request using trusted
built-in registry membership. Only these five handlers qualify for replay:
`stats`, `comm.thread`, `comm.inbox`, `comm.unread`, and `comm.delivered`. Each is
read-only with respect to domain state for every argument combination. Singles,
parallel batches, and chains qualify only when every operation qualifies,
including a chain of three `comm.thread` calls. References to previous results
do not widen this set. Empty, malformed, mixed, mutating, unknown, custom, and
mounted requests do not qualify. `help=true` and `dry_run=true` do not grant
exceptions for other verbs. Unclassified forwarding entry points, including CLI
callers, retain the default no-replay policy. The policy is local to the bridge;
the wire protocol is unchanged.

Only EOF/reset after a complete request write enables this read-only replay.
The bridge can make at most two additional completed attempts, with 100 ms
spacing and a single replay window of at most ten seconds, capped by the
original caller deadline and any active reconnect deadline. Missing-socket
polls consume time but not completed-attempt allowance. Cancellation prevents
another replay but does not abandon an already-admitted exchange. Malformed
frames/JSON, response timeouts, protocol mismatches, and explicit errors are
not replayed. An identity rejection after a lost response remains terminal;
exhaustion or later missing sockets never permit lifecycle recovery or local
fallback after the original complete write.

Ordinary audit accounting can repeat for a replayed read; no domain mutation
is repeated. The same request-group id remains correlation, not deduplication.
Read results can reflect a later snapshot. Every fully written request outside
this closed opt-in set retains Amendment 3's terminal ambiguity rule. A lost
response alone establishes neither daemon death nor a socket handover, so this
policy does not attribute unexplained server-side disconnects to restarts.

### Verification obligations

Tests cover absent/refused live-owner reconnect, listener closure before drain,
live-owner preservation, graceful exit without boot-lock inversion, changed
ownership during exit, rename-safe cleanup, trusted whole-request classification,
read replay limits, deadline/cancellation, terminal framing/protocol failures,
and a lost mixed batch whose mutation must commit only once.

The performance harness `scripts/perf/socket_handover_bench.py` offers actual MCP
`stats()` requests every 50 ms during ten isolated stop/start cycles. Run the
same harness against base and candidate binaries and report both failure counts
per restart and the longest failure gap in milliseconds, even when the target
of zero failures is missed. Compilation, tests, and these measurements are
separate gates; source inspection does not establish their results.

## Amendment 10 (2026-09-22): long-poll allowance and one classified read resend

A request containing `comm.inbox` long polls uses
`max(configured_read_timeout, sum(effective_wait_ms) + 5 seconds)` for its
request-read scope and otherwise unscoped socket exchange. An absent/zero wait
adds no allowance. Valid literal waits use the handler's bound of 30,000 ms;
invalid literals add no allowance. A chain reference to a not-yet-known wait
reserves the handler maximum. The 100-operation parser cap bounds the derived
allowance to 3,005 seconds; the configured read ceiling remains bounded at
3,600 seconds. A single ceiling poll therefore has a 35-second default budget.
The fixed margin covers transport and response work beyond the intentional
wait. An earlier caller deadline always wins; nested dispatch cannot renew it.

Both CLI built-in forwarding and MCP forwarding may resend a fully written
request **once** after EOF/reset loses its response, provided every parsed
operation is `Read` according to the shared `classify_operation` authority.
MCP also requires trusted canonical pack registration, so custom and mounted
handlers cannot obtain this permission by adopting a known name or category.
Unknown, mixed, Write, malformed, and empty requests are ineligible, including
requests that attach `help` or `dry_run` to a Write operation. The conservative
unclassified forwarding entry point retains its no-replay behavior.

This replaces Amendment 9's two additional completed attempts, five-name
MCP-only opt-in, and ten-second replay window. There is one resend attempt,
preceded by a 100 ms delay, within one request allowance plus that delay and
capped by any original caller or active reconnect deadline. A missing socket
consumes that attempt too. Cancellation prevents another attempt. No replay
failure can enable local dispatch or lifecycle recovery after the original
full write. Request identity, config fingerprint, namespace and request-group
id stay unchanged; the id is correlation, not deduplication. A later read can
observe a later snapshot and repeat incidental audit/cache effects.

Timeouts remain terminal `ParseFailure`; malformed responses, protocol or
identity mismatches, and explicit errors remain nonretryable. A lost mutation
response retains the existing ambiguity error and never triggers redispatch.
See [daemon lifecycle](../../crates/khive-mcp/docs/api/daemon-lifecycle.md#long-poll-deadlines-and-one-read-replay-3045)
for transport and regression details.

## Amendment 11 (2026-09-23): demand-driven daemon retirement and drain

The daemon deliberately outlives the client that starts it, so a warm process can serve many
callers over its lifetime. Today that lifetime has no upper bound for an automatically started
daemon: the accept loop in `crates/khive-runtime/src/daemon.rs` waits only on SIGTERM and
SIGINT, and nothing in it ever asks whether the daemon is still needed once the client that
spawned it is gone. This amendment gives every daemon an explicit, immutable launch mode, gives
the automatically started mode a retirement path, and states how retirement interacts with
in-flight work, explicit transactions ([ADR-005](ADR-005-storage-capability-traits.md)), and the
events daemon.

This amendment covers lifetime and drain only. Cross-request client ownership (#1933) and
distinguishing two connections of one authenticated subject (#2792) are decided separately: the
ownership contract is gated on an inventory of externally reachable retained state, and #2792
requires measuring the actual conversation-to-session mapping first. Neither gate blocks this
amendment, because until the ownership contract exists a demand daemon holds no credential-bearing
background work at all (see Credential-bearing background work below).

### Launch mode

A daemon process now carries an immutable lifetime mode, selected at launch and never inferred
afterward from parent liveness, a supervision marker, or the executable's location:

- **Demand.** Every automatic-spawn path selects this mode explicitly. A demand-mode daemon is
  eligible for idle retirement, as defined below.
- **Persistent.** The default for an explicitly invoked `mcp --daemon` and for every supervised
  launch. A persistent daemon keeps today's signal-only lifetime; this amendment does not
  change it.

An automatically started replacement permitted by [ADR-185](ADR-185-daemon-rendezvous-ownership.md)'s
bounded takeover (its Amendment 1) remains demand mode even while a supervision marker is
present: a marker records supervision, not launch mode, and this amendment changes neither
marker ownership nor starter arbitration. The mode, the daemon's instance generation, its
effective idle interval, and its shutdown reason are exposed through the daemon's existing
lifecycle diagnostics.

The mode travels on the daemon's command line. `mcp --daemon` gains `--lifetime <demand|persistent>`;
when the flag is absent the mode is persistent, so an explicit invocation and every supervisor
configuration written before this amendment keep today's behavior. Automatic spawn has a single
construction point, the argument builder in `crates/khive-mcp/src/daemon/launch.rs` that both CLI and MCP
forwarding reach, and that builder passes `--lifetime demand`; the bounded-takeover replacement
spawns through the same builder when it is implemented. The mode is not read from the environment:
a spawned daemon inherits its caller's environment, so a variable set for one client would silently
change unrelated launches, while a command-line value is visible to operators and to the
process-identity check. The daemon parses the value once at startup and never re-reads it.

Conformance requires:

1. a test on the argument builder asserting that the spawned command line carries
   `--lifetime demand`;
2. a test asserting that a daemon started without the flag reports persistent in its lifecycle
   diagnostics and one started with `--lifetime demand` reports demand;
3. the command-line identity check that recognizes a khive daemon (`argv_is_khive_daemon`) still
   recognizing both forms, with a test for each;
4. every future automatic-spawn path going through the same builder; a second place that
   constructs a daemon command line is a defect against this amendment.

### Credential-bearing background work

A demand-mode daemon starts no background component that acts with credentials or an actor
identity captured at startup: no channel loops (inbound polling or outbound delivery) and no
schedule execution. Those run only under a persistent daemon, started explicitly with
`mcp --daemon` or by a supervisor, until the client-ownership contract (#1933) defines who owns such
work once the client that caused the spawn is gone. An automatically started daemon inherits its
caller's environment and working directory, and later clients of the same user can reuse its socket,
so without this rule background mail or schedule work could keep running under a departed client's
credentials and identity. A user who wants an email channel or scheduled work therefore starts the
daemon explicitly. A demand daemon whose configuration declares a channel or schedules logs each
skipped component at startup and names it in its lifecycle diagnostics, so the difference is
visible rather than silent.

### Idle eligibility

A demand-mode daemon retires after 1,800 seconds (30 minutes) of quiescence, configurable to any
positive duration. This default is a disclosed placeholder, not a measured value: no cold-start
or reuse-cost measurement supports it today. The measurement that replaces it records two things:
for each supported auto-spawn path, the cold-start latency from spawn to service readiness; and,
from the lifecycle diagnostics of demand daemons in representative use, the distribution of gaps
between the end of one admitted request and the next admitted request on the same daemon. The
default then becomes the smallest interval that covers the 95th percentile of those gaps, so that
a retired daemon's next cold start is the exception rather than the rule; if cold start is cheap
enough that paying it on most reuses is negligible, a shorter interval is preferred. Until that
measurement exists, the value ships only as a configurable, clearly labeled placeholder, and this
amendment records no committed number.

The idle clock starts at service readiness, not at process start. Service readiness is the point
at which the daemon has bound its socket, published its pid file, started the background
components its mode permits (events-daemon supervision in both modes; channel loops and schedule
execution only in persistent mode), and installed its
SIGTERM and SIGINT handlers. See Signal handling below for a gap in the current source between
publishing the pid file and installing those handlers, and for the change that closes it; until
that change lands, readiness as defined here is not yet reachable, and the idle clock would
otherwise inherit the same unguarded window.

The clock restarts after each admitted external request and its cleanup finish. Readiness
probes, diagnostics-only probes, resource-lease heartbeats, checkpoint ticks, and an empty
maintenance poll do not restart it; an ordinary application request, including an admitted long
poll, does. A client's presence with no retained state does not by itself hold the clock open.
In-flight requests, admitted writes, unsettled workers, retained explicit transactions, and a
retained read-write connection handle (which keeps its writer permit for its whole handle
lifetime under [ADR-005](ADR-005-storage-capability-traits.md)) all prevent quiescence.

Background work must declare whether it is expendable cache maintenance or a service obligation
before it can inhibit retirement. A declared service obligation keeps a demand daemon serving past
its idle interval, with the diagnostic surface naming the obligation. Scheduled delivery and inbound
channel loops never hold a demand daemon open, because a demand daemon does not run them; a
deployment that needs them, or guaranteed continuous service, uses persistent mode. Unknown component state does not count as idle: a configuration is
eligible for default idle retirement only once its component inventory is complete.

### Admission and drain

Retirement is a one-way transition: Serving, then Draining, then Stopped. The idle check and
request or resource admission share one synchronization boundary, so an arriving request either
wins admission, which postpones idle exit, or receives a refusal before dispatch; no request can
arrive in an untracked gap between the two. An accepted connection whose frame never finishes
arriving, or a response write that never finishes, has a bounded transport budget and cannot by
itself hold retirement open indefinitely.

At voluntary idle retirement the daemon closes its listener before it drains, keeps its
rendezvous identity until settlement completes, and stops starting new background work. No
permit, transaction record, or ownership reference is released before its worker has actually
settled. An admitted write or an admitted transaction-control statement is never interrupted,
turned into a retryable cancellation, or abandoned because the drain interval has expired. If
outstanding work is still running when the drain interval expires, the daemon stays visibly
draining and keeps its rendezvous rather than exiting or admitting a replacement writer over it.
The drain interval bounds unused service retention, not the execution time of admitted work.

This is a bound on unused capacity, not a change to how the daemon shuts down on a signal or to
how a crash is handled. This amendment adds no durable write handoff and does not change the
existing mutation-ambiguity or read-replay rules.

### Explicit transactions

An explicit deferred read transaction inhibits idle retirement while its own maximum age keeps
it active; the existing maximum-age, cancellation, and handle-drop rules are unchanged. This
amendment adds no wire transaction API and does not treat multiple calls on one SQL handle as
multiple external requests. An admitted transaction-control statement always settles before
cleanup: retirement may close an abandoned read transaction after `BEGIN`, but it never
interrupts a `COMMIT` that is already executing.

### Events daemon

The main daemon registers `supervise_events_daemon_with_readiness` as the tracked
`events_daemon_supervision` task (`crates/khive-mcp/src/serve/startup.rs`). The supervisor
(`crates/khive-runtime/src/events_split/events_daemon.rs`) spawns the events-daemon subcommand when its socket
is unreachable and no child handle is retained, subject to its respawn interval. It polls the retained
child before each socket probe. A successful exit poll reaps the child; a poll error retains the
handle for a later attempt unless the OS reports `ECHILD`, which clears the handle without signalling
its PID. The main daemon cancels its shutdown token before waiting for tracked tasks
(`crates/khive-runtime/src/daemon/boot_guard.rs`). When the supervisor reaches its cooperative shutdown
branch, it attempts to kill and wait for its retained child; both results are currently ignored.
Dropping or cancelling the supervisor future does not run that cleanup. A pre-existing events daemon
it did not spawn is left alone. The code bases its kill policy on SQLite durability rather than
volatile child state; these paths do not guarantee reaping after permanent errors or cancellation.
[ADR-170](ADR-170-events-daemon-split.md), which describes this split, is proposed, not accepted.

Retirement must classify what the events daemon is to this process before deciding what to do
with it, at shutdown and at idle retirement alike:

1. **An exclusively owned demand child.** One this process spawned, with nothing else depending
   on it.
2. **An independently supervised service.** Running under its own supervisor, outside this
   process's control.
3. **A shared demand service.** A child that other consumers also depend on.

For an independently supervised service, this process changes nothing: the service's own
supervisor owns its lifetime. For a shared demand service, this process releases only its own
usage and lets the child retire under its own idle protocol once every consumer is gone; this
process never kills it.

For an exclusively owned demand child, this amendment changes today's behavior. Instead of an
unconditional kill, the main process stops routing new work to the child, waits for any event
handoff it has already acknowledged to reach the durability point the split already promises,
and only then asks the child to stop and reaps it. This is a change from the current
unconditional kill on shutdown, and it ships as its own bounded, separately verified change; this
amendment states the target contract, not an implementation. Idle retirement of the main daemon
may ship ahead of that change for configurations where the events daemon runs supervised or
shared. A configuration where the main daemon exclusively owns its events child is not eligible
for default idle retirement until the drain-request change for that child lands.

### Auto-spawn working directory and background actor resolution

The helper that auto-spawns a demand daemon inherits its caller's working directory today and
does not yet select a neutral one. A later change to use a neutral working directory must account
for more than configuration discovery: the daemon also resolves its own actor identity for the
background work it originates, such as channel loops and schedule execution, from a project
directory tied to its working directory, the same resolution tier every process uses when no
actor is set explicitly (`crates/khive-mcp/src/serve.rs`). A neutral working directory introduced
without an explicit, equivalent actor selection would silently change which actor that background
work runs as. Under this amendment a demand daemon runs no such work, so the question becomes live
only when the client-ownership contract (#1933) admits it in demand mode. This amendment does not
change the auto-spawn working directory; it records this so a later change treats the daemon's own
background-work identity as part of its scope, not only the client's.

### Signal handling

Two gaps in today's signal handling affect this amendment and are named here rather than folded
into its decision.

The first is a startup window. At current source, the daemon binds its socket, publishes its pid
file, and starts its background components before its SIGTERM and SIGINT handlers are installed;
installation happens only when the shutdown path is first polled, later in startup. A signal
arriving in that window has the platform's default disposition: the process exits with no drain
and no cleanup, leaving its pid and socket files behind and orphaning any events child already
spawned. Pull request #3091, open for review, closes this window by moving handler installation
earlier, before the pid file is published; that is also the ordering this amendment's definition
of service readiness depends on, above. Until that pull request merges, readiness as defined
above is not yet reachable in the running code, and the idle clock would inherit the same window.

The second is a repeat-signal gap, independent of this amendment's decision. Once installed, the
signal handlers replace the platform default for the rest of the process, so a second SIGTERM or
SIGINT sent while the daemon is already draining is captured and has no effect; only SIGKILL can
force an exit at that point, and SIGKILL skips cleanup. This is filed as its own defect, issue
#3119, and is referenced here, not decided: this amendment does not change what a repeat signal
does during drain.

### Text this amendment supersedes

This amendment supersedes three sentences above, for demand-mode daemons only; a persistent-mode
daemon keeps the original behavior exactly as written.

- "serves request frames against that warm registry until it receives SIGTERM/SIGINT" no longer
  describes a demand-mode daemon, which now also retires on quiescence.
- "Cold start is paid once per machine-uptime, not once per reconnect" no longer holds
  unconditionally for demand mode, where a retired daemon pays cold start again on its next
  spawn; persistent mode is unaffected.
- "for now the daemon exits on signal and is re-spawned on demand" is superseded for demand mode
  by the retirement policy above; it remains accurate for persistent mode, which this amendment
  does not change.

### Verification obligations

A second client attaches to a demand daemon after the first client that spawned it exits, and the
daemon keeps serving. A quiescent demand daemon with no service obligation retires within its
configured interval; an identical persistent daemon does not. Probe and empty-maintenance traffic
do not restart the idle clock; an ordinary request does. A daemon with a declared service
obligation stays serving past its idle interval and names the obligation in its diagnostics. A
demand daemon whose configuration declares an email channel and schedules starts neither, reports
both as skipped, and retires on its idle interval; the same configuration under a persistent daemon
starts both. A
request racing idle retirement either completes normally or receives a refusal before dispatch,
with no request unaccounted for. A slow admitted write or an open explicit transaction is never
interrupted by idle retirement, and a write that outlives the ordinary drain interval still
reaches its own commit or rollback boundary without duplication. An exclusively owned events
child is drained and reaped only after its acknowledged handoffs settle; a shared or
independently supervised events child is never killed by this process.

## Amendment 12 (2026-09-28): bridge-local fallback diagnostics

**Status: Accepted (2026-09-28).** Addresses #3495. This amendment defines a read path for the five
bridge-process fallback counters and the strict-violation counter specified by Amendment 2.
It does not create daemon-owned counters or change KG `stats()` or the daemon's
`db_diagnostics.process` identity.

### Bridge-only operation and response

The stdio bridge answers `request(ops="bridge.diagnostics()")` before daemon forwarding or
local pack dispatch. `bridge.diagnostics` is a bridge control operation, not a pack verb. This
one operation is an explicit exception to ADR-016's pack-registration rule and is discoverable
in the bridge's `request` tool description. It reads only the stdio bridge's process memory:
no runtime, database, blob store, socket, daemon, or executable-file check is needed to answer
it. The daemon does not implement this operation, and the bridge never puts it in a daemon
frame. A direct daemon request naming it is refused.

A successful single-operation response uses the ordinary request success envelope. Its result
has these fields:

- `bridge_instance_id`: an opaque canonical UUID minted once per stdio bridge process image
  before it handles requests. It is the counter-generation identifier. An in-place re-exec
  mints a new value even when the PID and operating-system process start time stay the same.
  The full 36-character value survives Agent, Verbose and Human presentation and JSON, auto
  and table formatting. This field is a narrow exception to ADR-045's usual Agent-mode
  shortening of `*_id` fields: an eight-character prefix cannot reliably distinguish
  counter generations.
- `pid`: the serving stdio bridge's process ID, for operator correlation; it is not the
  counter-generation key on its own.
- `fallback_reasons`: an object with exactly the five Amendment 2 keys `config_mismatch`,
  `namespace_mismatch`, `no_socket`, `parse_failure` and `protocol_mismatch`, each a
  nonnegative count sampled once for this response. A reserved reason with no current
  production fallback remains present with count zero.
- `fallback_total`: the checked sum of those five sampled counts, not a separately sampled
  counter. If that sum cannot be represented, the read refuses rather than wrapping.
- `strict_violations`: the bridge-process count of Amendment 2's illegitimate fallback
  events observed while strict mode was active, sampled for this response. It is separate
  from the five-reason sum.

The five atomics are read individually, so a concurrent fallback can appear in a later read;
`fallback_total` still equals the sum of the five counts in its own response. The result does
not report `counters_since`: a wall-clock instant would not establish an atomic sampling
boundary or survive an in-place re-exec as a continuous window. Consumers compare
`bridge_instance_id` before comparing counts across reads; a new ID resets the window.

`bridge.diagnostics(help=true)` is also answered by the bridge and returns the local
operation's argument and result schema together with ADR-016's shared
`identifier_resolution` object. Any other operation argument is invalid. The ordinary JSON,
auto and table renderers and presentation modes render the same named result fields;
formatting never substitutes daemon `db_diagnostics` or drops or shortens the generation ID.
The operation is single-only: if it appears in a parallel batch, a chain, or a bracketed batch
of chains, the entire request is rejected as invalid parameters before any sibling operation
executes or forwards. This is a narrow exception to ADR-016's ordinary per-operation batch
failure isolation. `save_to` is rejected before creating a sink, so the diagnostics read
remains store-free. These restrictions also apply to `help=true`.

With `plan=true`, a single `bridge.diagnostics()` or
`bridge.diagnostics(help=true)` is parsed without execution and reported as
`known=true`, `pack="bridge-control"` in the normal plan stages. The latter is a
plan-only catalog label, not a registered pack. This bridge-control plan path performs no daemon
forward or executable-file check. An otherwise syntactically valid mixed plan may list this
stage as known, because plan checks syntax and catalog membership, not execution admission;
executing the same mixed request still refuses the whole request. ADR-016's ordinary
`plan=true` envelope exclusions continue to apply.

### Strict mode and reset behavior

`KHIVE_DAEMON_STRICT=1` still rejects every ordinary request that would take a local
fallback, using Amendment 2's structured refusal. The bridge-local diagnostics operation is
not a fallback: after normal request parsing and cancellation admission, it is intercepted
before `BridgeExecutable::check`, `fallback_or_reject` and the forwarding path. It remains
readable when the on-disk bridge executable has been replaced but re-exec has not yet
occurred; an ordinary request retains the existing executable-check behavior. Reading
diagnostics neither increments a fallback reason nor weakens strict refusal for any other
operation. A strict-mode outage can therefore report counters from earlier refused requests
without contacting a daemon. This amendment narrows Amendment 2's proof statement:
"strict mode active and fallback count zero" proves daemon dispatch for every **served
ordinary request**; this explicitly bridge-local control read is not daemon-dispatched and
must not be used as evidence of an ordinary request's route.

A new bridge process image starts all six counters at zero and mints a new
`bridge_instance_id` before admitting its first request. An in-place re-exec has that same
reset behavior; preserving PID or OS start time does not preserve a counter window. The
resumed bridge must not reuse an ID supplied by the prior image or by the client.

### Rejected alternatives and acceptance

Appending a bridge section to `db_diagnostics()` was rejected because the forward must
succeed before the counters could be read, precisely when the bridge may need diagnosis; it
would also mix bridge state beside `db_diagnostics.process`, which identifies the daemon
serving that request. A transport sidecar on every response was rejected because an
operator-only reading does not justify enlarging every ordinary response or changing
response composition. This standalone operation changes no pack response-composition rule
in ADR-023, so that ADR is unchanged.

Acceptance requires a no-daemon bridge to answer this operation after a real ordinary
`no_socket` fallback and report `no_socket > 0`; `fallback_total` equals the sum of the five
reason fields in that same response. During a strict-mode outage, an ordinary request is
refused and counted while this operation still answers; a must-deny control proves another
ordinary request is still refused, and a mixed diagnostics-plus-write batch is refused
before the write. A single `plan=true` reports the bridge operation as known without
executing it; `help=true` carries `identifier_resolution`. Default Agent presentation and
auto/table formats preserve the full `bridge_instance_id`. A replaced bridge executable
does not mask this memory-only read. A real in-place re-exec leaves PID continuity possible
but changes `bridge_instance_id` and restarts all counts. A daemon-side frame counter or log
proves that the diagnostics operation, including help and formatted reads, was never forwarded.

## Amendment 13 (2026-09-27): daemon ownership follows claimed stores

**Status**: Accepted.

### Context

ADR-049's accepted daemon singleton is scoped to one socket/PID rendezvous. Its
HOME-derived socket path and the matching boot/recovery lock do not identify a
SQLite store (§4 Socket protocol and Amendment 3 of this document).
Two serving processes with different HOME values can therefore open the same
absolute `[[backends]].path`. ADR-028 already deduplicates configured SQLite
backends by canonical pathname (`docs/adr/ADR-028-pack-scoped-backends.md`,
§Why deduplication by canonical path); that pathname is the ownership key for this amendment.

### Decision

An OSS serving daemon claims every distinct file-backed SQLite pathname in its
effective topology before opening SQLite or running migrations. The claim is
independent of HOME, socket, PID file, and recovery lock. A declared backend
topology supplies its effective SQLite paths; an implicit single backend
supplies the resolved database path. An in-memory target supplies none. Sort
and deduplicate canonical paths before claiming. If a claim's derived lock
sidecar pathname is itself a configured database pathname, refuse the whole
boot before opening any sidecar for writing; after opening each sidecar but
before locking or truncating it, refuse when the opened descriptor's
`(device, inode)` matches any configured database identity observed at claim
time or its hard-link count exceeds one (a configured database missing at
claim time remains covered by the pathname preflight). A database whose own file
name has the sidecar form (`.<name>.khived.lock`) refuses the boot at the same
preflight. An existing sidecar longer than the 64-byte pid record, or starting
with the SQLite header, is refused before truncation and left untouched. If one claim is unavailable
or contended, abort the entire boot and release earlier claims. Hold all
claims through serving and shutdown. Never unlink a lock sidecar. A contender
refuses even when the holder PID is temporarily unavailable; PID text is only
best-effort diagnostic evidence. Ordinary non-daemon stdio/local writers
retain their existing coordination and are outside this daemon singleton
guarantee. With the HOME boot lock held, an identified daemon already serving
the same socket retains ADR-049's socket-owner refusal before store claims are
attempted. This bounded, read-only protocol probe neither opens SQLite nor
changes the rendezvous. An absent or unidentified socket proceeds to the store
claims; those claims still precede every SQLite open and migration. The normal
socket/PID boot fence remains authoritative after construction.

Each claim uses a persistent `.DATABASE.khived.lock` sibling of the canonical
database pathname. After the sidecar lock is held, freeze that canonical path
as the _only_ path passed to SQLite. Re-resolve the configured spelling once
for validation before opening; if a symlink now targets another path, refuse
and name both the claimed and current paths. Validation must not replace the
frozen open target.

For a regular file present at claim, record `(device, inode)`. For a
missing writable target, keep a Missing claim state, create/open the frozen
path under the held lock, and bind its newly observed `(device, inode)`.
Missing read-only targets refuse without creation, and a read-only claim never
creates a missing parent directory. A created writable database has mode 0600
before the umask; opening a pre-existing database preserves its permissions.
SQLite's Unix VFS gives newly created WAL and SHM files the database's mode.
The store-claim layer does not change permissions of existing databases or
sidecars. Re-stat each canonical path
immediately after its SQLite backend opens and before schema preparation, then
again before serving; observed identity drift fails boot. Pass the bound file
identity into both implicit and declared-backend pool construction. On verified
64-bit Linux and macOS, the binary installs an immutable forwarding observer for
the bundled Unix VFS `fstat` syscall at process startup, before any SQLite file
I/O. A claimed writer open must observe the actual descriptor identity and match
every successful observation to the held claim before any SQL, identity-row
initialization, or WAL setup. No observations or any mismatch refuse boot,
including when the original pathname is restored after a foreign file opens.
The daemon's claim step, not SQLite, creates a missing writable database: after the sidecar
claim it opens the file with create relative to the claimed parent directory and binds that
descriptor's identity, and claimed SQLite opens then disable `SQLITE_OPEN_CREATE`, so SQLite
opens only a file the claim created or found.
Library hosts must call the unsafe startup initializer under its documented
precondition; claimed opens never install it lazily. Other Unix ABIs and
unverified VFS implementations refuse claimed opens. The forwarding callback
neither closes nor denies SQLite's descriptors, preserving native POSIX lock
handling. The final path checks remain additional startup guards.

This check precedes pool SQL and WAL changes, not all native open-time I/O.
SQLite's macOS msdos/exfat path can write to an empty file during native open;
trusted nonmutating autoextensions must remain in place. The observer does not
promise protection from that filesystem-specific raw write, mutating
third-party autoextensions, or later replacement of the VFS syscall table.

The guarantee is **per stable canonical pathname among participating daemon
boots**, not per physical inode across distinct hardlink names. Symlink
spellings that resolve to one stable pathname converge. A hardlink's distinct
pathname gets a distinct sidecar, so another daemon can claim it even while
the first daemon serves the same inode; hardlink aliases are unsupported. The repository test
`store_guard_hardlink_names_remain_independent_unsupported_aliases` in
`crates/khive-runtime/src/daemon.rs` demonstrates the separate claims. This
is a property of the sidecar scheme, not a claim about SQLite's support for
multi-link database files. A sidecar hardlinked to any file is refused before
lock or truncation. Replacing a database at the same canonical pathname does not erase its
persistent sidecar claim.

For operators, the sidecar belongs to the store's directory, not to its data. Copying a store
copies the database by SQLite's own rules and never the sidecar; the destination's first daemon
creates its own. A copy that hard-links the sidecar (`cp -l`, a hard-link backup) is refused at
boot by the link-count check. Restoring or replacing a store at the same path is done with every
daemon on that store stopped, and leaves the sidecar in place; the next boot binds the new file.
Never delete a sidecar while a daemon may be serving the store, because a second boot would then
lock a new sidecar while the first still holds the old one. With every daemon stopped, deleting it
is harmless: the next boot recreates it.

### Acceptance and controls

1. Distinct HOME rendezvous with one absolute SQLite store: the second daemon
   refuses before constructing SQLite and names the contended store.
2. Multiple declared backends with one shared secondary canonical path: the
   second daemon refuses the full topology and drops any earlier claims.
3. A configured symlink retargeted after claim is refused before open, with
   both paths named; mutation of alias revalidation turns its named test red.
4. A canonical file replaced after claim/binding is refused on the post-open
   `(device, inode)` check; a no-op post-open assertion turns its named test red.
5. A fresh writable database is created and identity-bound only after its
   sidecar claim; a missing read-only target is not created. A serving daemon
   retains every claim until shutdown.
6. A second claim through a distinct hardlink name succeeds in the fixture,
   documenting the excluded alias rather than accidentally promising safety.
7. A claim set whose derived lock sidecar is another configured database
   refuses the whole boot before any sidecar is opened for writing; an opened
   sidecar whose descriptor identity matches a configured database or whose
   hard-link count exceeds one refuses before lock or truncation.
   `store_guard_refuses_a_database_at_another_stores_lock_sidecar` goes red if
   the pathname preflight is dropped;
   `store_guard_names_a_configured_database_matching_the_opened_sidecar` and
   `store_guard_refuses_hardlinked_sidecar_before_lock_or_truncate` cover the
   descriptor-identity and hard-link arms, respectively.
8. A database named like a sidecar, or an existing sidecar holding a SQLite
   header or more than 64 bytes, refuses before truncation, while a short
   pid-only sidecar is reused. A read-only claim whose parent directory is
   missing creates nothing.
   `store_guard_refuses_to_truncate_a_sidecar_path_that_holds_other_data`,
   `store_guard_reuses_a_short_pid_only_sidecar_after_a_restart`,
   `store_guard_refuses_a_database_named_like_a_lock_sidecar` and
   `store_guard_read_only_claim_refuses_a_missing_parent_without_creating_anything`
   cover them.
9. The descriptor observer. A writer, a standalone reader and a later pooled reader whose open
   observes a foreign file refuse before any SQL
   (`standalone_writer_refuses_foreign_descriptor_before_sql`,
   `standalone_reader_refuses_foreign_descriptor_before_sql`,
   `later_pooled_reader_refuses_foreign_descriptor_before_sql`); opening without the held identity
   turns each red. A daemon claim refuses a foreign pool before identity-row or WAL setup
   (`daemon_claim_blocks_foreign_pool_before_identity_or_wal_initialization`); dropping the claims
   from the claimed backend open turns it red. An open with no successful observation refuses
   (`observation_without_native_descriptor_evidence_refuses`); accepting a zero observation count
   turns it red. A mismatch refuses even after the original pathname is restored
   (`descriptor_claim_refuses_foreign_open_even_after_pathname_is_restored`); skipping the mismatch
   record in the observer turns it red. An observer that did not install, which is how an
   unverified ABI or VFS presents, refuses every claimed open
   (`claimed_pool_without_process_startup_observer_refuses`); removing the initialization check
   turns it red. No test forces the individual VFS, library-version or replaced-`fstat` detections
   on a supported host, and on other targets only the refusing installer is compiled.

The HOME boot/recovery lock still serializes startup and client recovery for
one rendezvous. This amendment adds a separate store claim; it does not
replace that lock or extend daemon ownership to non-daemon writers.

## Amendment 14 (2026-10-12): strict forwarding by default and a named slow-daemon outcome

**Status**: Accepted 2026-10-12. Decisions 2 and 3 are accepted; Decision 1 is withdrawn (see
Decision 1).

### Context

A stdio host that forwards to the daemon (Decision §2) builds the same store-backed registry the
daemon builds, before it reads its first request: it opens every configured SQLite backend, applies
migrations and pack schema plans, and runs the boot repairs. Every pooled connection maps its store
(a 1 GiB memory-map window by default) and carries its own page cache. The client then forwards
every request, so on a host with a running daemon that work serves nothing. Measured before the
legacy quarantine repair was skipped in forwarding clients, a client start took about 8 seconds
and pulled the comm and main stores through the page cache; the repair skip removes the scan, not
the opens, migrations and maps.

A forwarding client serves a request itself in exactly these cases today
(`crates/khive-mcp/src/server/bridge.rs`, `request_with_forward`):

- the request carries `save_to`, which the daemon frame cannot carry, so it is always local;
- the main backend is in memory, so no daemon can own it;
- forwarding returned no response and Amendment 2's policy admitted local dispatch: a `no_socket`
  outcome outside strict mode.

`bridge.diagnostics` (Amendment 12) and request planning read process memory and the pack verb
catalog, not a store.

Separately, two client outcomes that mean "the daemon is alive but did not answer in time" carry no
stable reason today. The boot-fence identity probe that times out after cold-boot quiescence
returns a prose-only error. A response read that times out after the frame was fully written is
classified internally as a parse failure (`crates/khive-mcp/src/daemon/forward.rs`) and returns the
same error as a dropped connection ("daemon response lost after request was sent"). A caller cannot
tell a slow daemon from a dead one, or count either.

### Decision

**1. Lazy store-backed registry: withdrawn.** As proposed, a forwarding client whose main backend
is file-backed would have opened no SQLite backend at boot, built its catalog from pack metadata,
and built the store-backed registry once, on its first local request. Measured after the per-
connection page cache was capped at 8 MiB and the legacy quarantine repair was skipped in
forwarding clients, a forwarding client boots in about 90 ms (warm, process start to its last boot
line) and holds 17 MB physical footprint idle and 17 to 18 MB after serving requests, against
303 MB before those two changes. At that cost the lazy build saves too little to justify building a
verb catalog without pack runtimes and keeping the client's daemon identity equal to the eager
path's.

What the decision would still have bought is a client that never creates a store the daemon owns
and never runs migrations beside the daemon. A narrower form that keeps only that property needs a
no-create open mode through the pool and backend open for every configured store; migrations and
pack schema plans skipped in forwarding clients; a client that starts the daemon first when a store
is absent, so that the first run on a fresh host still works; and an audit of the other boot-time
file writers (vector segment directories, the events sidecar, the attachment cutover). It is left
to its own change. Forwarding clients keep the eager build.

The narrower boot-admission property is proposed in [Amendment 15](#amendment-15-2026-10-19-conditional-client-boot-without-writer-admission).
That partial amendment is Proposed; it does not restore the withdrawn lazy-registry design or
change the status of this decision or Decisions 2 and 3.

**2. Strict by default when a rendezvous is present.** `KHIVE_DAEMON_STRICT` becomes a tri-state:
a truthy value is strict, an explicit false value (`0`, `false`, `no`, `off`) is not strict, and
unset means strict exactly when a daemon rendezvous was present when the request entered the
forward path, or is present at the fallback decision. A rendezvous is present when the client's
resolved socket path or PID file (`khive_runtime::daemon::socket_path`, `pid_path`) exists on disk.
The entry reading matters because recovery removes a dead daemon's socket and PID file before it
respawns: read only at the decision, a host whose daemon just died would look like a host that never
ran one. Under strict mode a request
that would fall back is rejected with its reason code, as Amendment 2 already specifies for
`KHIVE_DAEMON_STRICT=1`. With no rendezvous on disk, the quiet `no_socket` fallback of
Amendment 2 is unchanged. The sanctioned way to run without a daemon stays `KHIVE_NO_DAEMON=1`.

[Amendment 15](#amendment-15-2026-10-19-conditional-client-boot-without-writer-admission)
proposes retaining the earlier rendezvous observation when its new boot recovery can consume that
evidence before request entry. This limited observation-lifetime amendment is Proposed; the
accepted tri-state and refusal outcomes above remain in force.

**3. A named slow-daemon reason.** Both "alive but not in time" outcomes carry
`reason: "daemon_slow"` in the error data, with a `phase` field and the elapsed wait in
milliseconds:

- `phase: "before_dispatch"`: the boot-fence identity probe timed out. Nothing was written;
  `domain_disposition` is `not_committed`, and a retry is safe.
- `phase: "after_write"`: the response read timed out after the frame was fully written. The
  request may have been applied; `domain_disposition` is `unknown`. The no-retry rule of
  Amendment 3 is unchanged: a caller settles by reading state before any re-send.

The other post-write outcomes keep the existing error and gain a reason: a connection closed,
reset, aborted or broken after the write carries `reason: "daemon_connection_lost"`, and any other
read error or an undecodable response carries `reason: "parse_failure"`. The three post-write
outcomes become distinguishable, and all three stay terminal.
`daemon_slow` is a refusal, not a fallback: it never enters the Amendment 2 fallback set and is
not counted in `fallback_total`. `bridge.diagnostics` (Amendment 12) gains one field,
`daemon_slow`, an object with `before_dispatch` and `after_write` counts for this bridge process;
every existing field keeps its meaning.

### Acceptance

1. Withdrawn with Decision 1.
2. Withdrawn with Decision 1.
3. With a socket file present but refusing, a request is rejected with `no_socket` when
   `KHIVE_DAEMON_STRICT` is unset, falls back when it is `0`, and falls back quietly when neither
   socket nor PID file exists. With a PID file naming a process that has exited and no socket, a
   request is rejected with `no_socket` when `KHIVE_DAEMON_STRICT` is unset, although recovery
   removes that PID file before the decision.
4. A boot-fence probe timeout returns `daemon_slow` with `phase: "before_dispatch"` and
   `not_committed`; a post-write read timeout returns `daemon_slow` with `phase: "after_write"`
   and `unknown`; a post-write connection reset returns `daemon_connection_lost`.
5. `bridge.diagnostics` reports the two `daemon_slow` counts, and `fallback_total` is unchanged by
   them.

Each acceptance item names a test whose mutation (strict default removed, entry reading removed,
reason dropped) turns it red: `daemon/strict_default_tests.rs` for item 3 and
`daemon/daemon_slow_tests.rs` for items 4 and 5, which include a real in-process daemon whose
write is blocked by a held store lock.

### Alternatives considered

- **Strict for every forwarding client, unconditionally.** Simpler, but a client that has never
  had a daemon on the host, with spawning unavailable, would refuse instead of serving. The
  rendezvous predicate keeps that case on Amendment 2's quiet fallback.
- **Keep the eager build with smaller maps and caches in clients.** Lowers the resident numbers
  but still opens every store, runs migrations and schema plans, and races the daemon's boot on
  each client start. This is where forwarding clients stand after Decision 1 was withdrawn; the
  measured numbers are in Decision 1.
- **Forward `save_to` requests and write the sink in the client.** Removes the most common local
  case, but changes the daemon frame; it is compatible with this amendment and left to its own
  change.

### Out of scope

Daemon and client timeouts keep their current values; this amendment names outcomes, it does not
tune them. Daemon ownership of stores (Amendment 13) and the reconnect rules (Amendments 9 and 10)
are unchanged.

## Amendment 15 (2026-10-19): conditional client boot without writer admission

**Status**: Proposed. **Disposition**: partial amendment of Amendment 14, Decision 1's deferred
no-create and migration-free boot property, and Decision 2's rendezvous-observation lifetime when
the new boot recovery precedes request entry, for #5223. The base ADR remains Accepted. The lazy
registry design stays withdrawn. Decision 2's tri-state settings and refusal outcomes, Decision 3
and the other amendments remain in force. Dependent implementation does not merge before this
amendment's acceptance gate.

### Context and boundary

The eager registry is retained, including discovery, mount resolution, pack dependency and
collision checks, and every mounted pack's schema plan. Idempotent DDL still acquires a writer:
the writable pack-catalog path calls `apply_pack_ddl_statements_with_columns` for a nonempty plan.
Pool identity bootstrap, journal-mode setup, configured embedding-model registration,
vector-table preparation, the Events schema gate the audit binding reaches, blob-root creation and
direct events-sidecar open can also write before the first request, and diagnostic and attribution
files outside the stores are written as well. Skipping core migrations alone therefore does not
establish the deferred property.

This amendment proposes a boot-admission policy separate from configured store mode. A configured
writable store remains writable, with the same configuration identity, routes and capabilities.
Read-only **validation operations** during boot do not turn it into a configured read-only
snapshot. There is no new public request flag, built-in-only catalog or lazy registry.

### Decision 1: select the conditional boot once, preserve request forwarding

For a file-backed stdio forwarding client, select the conditional boot when a rendezvous is
present at boot, strict forwarding is not explicitly disabled, and `KHIVE_NO_DAEMON=1` is not
selected. A rendezvous means either the resolved socket path or PID file exists, as in Amendment
14. A daemon process and an in-memory main backend do not select this client policy.

An absent rendezvous, explicit strict false (`0`, `false`, `no`, `off`), or `KHIVE_NO_DAEMON=1`
keeps today's eager boot. Explicit strict true without a rendezvous still has its existing
request semantics; it does not, by itself, select this boot policy. Boot selection must not use
the broader `is_forwarding_client()` predicate, which does not establish these conditions.

Conditional validation reads the socket and PID paths without removing either. Stale-rendezvous
cleanup remains request-entry recovery, except when Decision 2's daemon-first startup itself
needs that recovery. Before such startup can consume the evidence, retain the selected boot's
positive rendezvous observation in the MCP host's state. It survives provisional-runtime teardown,
ordinary eager rebuild and every spawned forwarding task for that host; a task-local reading
taken only after boot is insufficient. End this observation's lifetime with that host, and do not
leak it into a separately booted host in the same process.

An explicit strict setting keeps priority. Otherwise the strict default is true if **any** of
the host's retained boot observation, Amendment 14's request-entry observation or the rendezvous
at the fallback decision is true. This is the limited Decision 2 observation-lifetime extension:
boot recovery cannot make an unset-strict client forget a daemon it already observed. Explicit
false remains non-strict; `KHIVE_NO_DAEMON=1` and requests local by design retain their routes.
The observation is host state, not a new setting or part of `config_id`.

Ordinary **boot** fallback below does not add any local **request** fallback, even after a failed
daemon-first launch clears both paths before the first request. `daemon_slow`, unknown post-write
outcomes, and all existing no-retry rules are unchanged.

### Decision 2: no-create validation, with explicit ordinary-boot exits

Under the conditional policy, open every configured store without creating its database or parent
directories. Retain physical-file, volume, path-trust and backend-identity checks. Do not bootstrap
a database nonce, change journal mode, apply core migrations or pack DDL, register configured
embedding models, prepare vector tables or store schemas through a writer, create a blob root, run
attachment conversion, or acquire a volume write lease during successful boot. No partly validated
runtime may become the serving registry.

Use the existing reader-side core compatibility check and pack declared-column validation for
all selected plans, including mounts. The pack check validates `PackColumnAddition` declarations;
an empty declaration list does no work. It does **not** prove that arbitrary SQL tables, indexes
or triggers exist, and this amendment neither prepares nor executes undeclared pack SQL nor claims
complete validation of it. The model-registry and boot-reached schema-gate checks below read
definitions and rows and prepare their own built-in statements without stepping them; no
validation DDL or DML runs against a configured store, and a reference definition is built only in
a private in-memory database. Eager plan collection and collision checks still run.

Each exit below is decided from a typed classification of what validation found, never from an
error variant or message text. Where an existing validator folds a missing prerequisite and an
incompatible one into one error, the check is split at its source. A result containing any
refusal class is a refusal, even when it also contains a missing prerequisite. A native query
error remains that error.

The following exits are explicit:

- A missing configured database, including a configured direct events sidecar, first asks the
  daemon to start through the existing startup/recovery protocol. The client holds no store
  writer, volume lease or recovery-lock descriptor while it does so: a client that took the boot
  recovery lock releases it before invoking that protocol, which takes the lock itself, and does
  not hold it while waiting for the daemon to become ready. After startup the client reacquires
  its boot lock, reads the rendezvous again, and repeats Decision 1's selection and the no-create
  validation from the start. A store still absent then takes today's ordinary eager boot under
  that lock.
- A core schema behind this build, missing declared pack columns or configured vector tables,
  or an incomplete attachment cutover requires ordinary eager boot. For an incomplete cutover the
  exit is the selection, not a promise that the ordinary repair succeeds; a corrupt cutover state
  may still refuse there. A configured writable database whose identity table is absent also
  takes that path; the conditional path does not mint a nonce or substitute weaker
  physical-identity checks. An identity table without its row, or with a malformed identity,
  keeps its existing error.
- The persistent registration population of a runtime is what ordinary boot registers today: on a
  writable backend, the models `configured_embedding_models` returns for that runtime's
  configuration (configured engines whose names parse as built-in model aliases, or, with no engine
  list, the configured embedding model and the additional embedding models). A model in that
  population without its active registry row takes ordinary eager boot, which registers it as today.
  Its active registry row is an `active` row whose canonical key equals the one ordinary boot writes
  for it (engine name, model id, key version and dimensions) under the upsert's own binding and
  comparison, a BLOB value, so a TEXT value with the same characters is a different key, and whose
  activation time is set, and every column of which holds a value that the column's declared
  affinity stores as it is: inserting the row's values into the private in-memory registry described
  below gives each column the same value with the same storage class. An update rewrites the whole
  record and applies each column's affinity again, so a row holding the real number 8.0 in the
  INTEGER `output_dim` column, which a store can carry while still reading normally and which the
  integrity check below reports as `ok`, would be changed by the upsert. Whether the upsert leaves
  such a row as it is
  depends on the registry's whole definition, so the check compares the definition rather than
  listing properties of it. In one consistent read snapshot, the registry table and every index and
  trigger attached to it, implicit indexes included, must equal the objects this build's registry
  DDL creates when run against a private in-memory database: object type, name and table compared
  exactly, and SQL text compared token for token, ignoring only whitespace and comments between
  tokens. The two creators of the table in this build, the registry DDL and the V1 schema, produce
  the same tokens. An added column, constraint, index or trigger, a changed definition under an
  existing name, and a temporary object in the reader's connection that names the registry are each
  outside that definition. With the definition matched, the reader also prepares the registry DDL
  and the registration upsert without stepping them, on a connection with foreign key enforcement on
  as every connection the source sets up has it (`crates/khive-db/src/pool/connection_setup.rs:300`,
  `:333`), so a statement SQLite would refuse when compiling it is found before boot is selected; an
  empty table elsewhere whose column references a registry column that is not a key leaves every
  registry object exact and makes the upsert refuse to compile there. In the same snapshot, `PRAGMA
  integrity_check` naming the registry table, which checks that table's records and every index on
  it, must return the single result `ok`. Definitions and stored values say nothing about index
  contents: a registry whose one-active index lacks the active row's entry matches every other
  condition here, and its upsert fails as a malformed image, while a quick check, which reads the
  index pages but does not verify that index content matches table content, reports `ok` there.
  For that definition the upsert supplies every
  column that requires a value and satisfies the status check, and on an existing key it sets only
  the status and an unset activation time, so in a registry that passes that integrity check an
  active row whose activation time is set and whose values its columns' affinities store as they are
  is left unchanged. Any other state takes ordinary
  eager boot, which creates, fills, reactivates or refuses as it does today: a definition outside
  the one above, a missing table, index or column among them, a failed preparation, an integrity
  check of the registry that reports anything but `ok`, no row with that key, an active row with
  that key and no activation time, an active row with that key holding a
  value its column's affinity would convert, a row with that key in another status, or, with no row
  carrying that key, an active row for that engine name under another key, which ordinary boot's
  insert
  refuses on the one-active-per-engine index. A store whose registry was created from text with
  other tokens is never admitted, because ordinary boot leaves an existing definition as it is; the
  acceptance below includes a long-lived store migrated through the supported history, so a rule
  that admits only freshly created stores fails it. A refused insert has the host result it has today:
  `from_prepared_backend`, which a single-backend host uses, returns the error, while
  `try_from_backend` and `from_backend`, which a multi-backend host uses for its default and
  per-pack runtimes, log it and continue. Conditional boot classifies none of these states itself
  and changes neither result. With every such model registered, conditional boot writes nothing to
  the registry. Every runtime the host assembles, default and per pack, follows this rule. A
  provider a pack registers under a name in that population replaces only the in-memory provider:
  ordinary boot registers the row before provider hooks run, so the model stays in the population
  and a missing row takes ordinary eager boot as it would without the pack. A read-only runtime, and
  a provider a pack registers under a name that parses as no built-in model alias, have no
  persistent registration obligation at ordinary boot, whatever rows the store already holds:
  conditional boot neither checks nor writes a row for them, and their provider binding is validated
  as ordinary boot validates it.
- A per-store schema gate that boot itself reaches is marked ready only after a reader check, in
  one consistent read snapshot, that its initializer would leave the store unchanged without
  refusing. Marking the gate ready is what keeps its initializer from running; the reader check
  alone does not. Today the audit binding reaches the Events gate during boot; its initializer runs
  `CREATE ... IF NOT EXISTS` for the events tables and their indexes, adds both operation
  attribution columns to an events table that has neither, and refuses one that has exactly one.
  For that gate the check requires three things: every table and index the initializer's DDL names
  exists as an object of that type; both attribution columns are present as the initializer's own
  column check sees them; and every statement of that DDL prepares, without being stepped, on a
  reader connection holding no temporary object. Each covers what the others cannot. Preparation
  succeeds for a `CREATE ... IF NOT EXISTS` whose object is absent, so it does not show the object
  exists. A name that exists does not show its statement is a no-op, because SQLite refuses some
  statements when compiling them: an index statement naming a virtual table is refused even when an
  index of that name already exists. The column check reads `pragma_table_info`, which does not
  list a generated virtual column, so a column declared that way counts as absent: the initializer
  would run its upgrade, and the upgrade's refusal is ordinary boot's. The check does not compare
  definitions: an index of an expected name on another ordinary table is one the initializer leaves
  as it is, and an events table whose attribution columns the upgrade added passes as a fresh one
  does. A state the initializer would change or refuse takes ordinary eager boot, which upgrades or
  refuses as today. Preparing these statements authorizes no execution of them against the store.
- A missing filesystem blob root, configured explicitly or resolved by default, takes ordinary
  eager boot, which creates it as today. Conditional boot opens an existing root without creating
  it and keeps hydration configured: it does not treat a missing default root as the absence of a
  blob store. Malformed blob configuration and path-trust refusal keep their errors.
- A direct events sidecar whose existing permissions require the ordinary hardening procedure
  takes ordinary eager boot. Conditional validation must not expose a sink that the existing
  permission policy would first have hardened.
- A newer or incompatible schema, malformed configuration, identity or volume change, path-trust
  refusal, or failed audit capability check retains its existing error. Such errors are not a
  general excuse to retry with more write authority.

Before an ordinary-boot exit, discard provisional boot state and its handles; do not mix a partly
validated catalog with a writable fallback catalog. This includes a process-lifetime cache entry
the provisional boot published, such as a direct events-sidecar backend: conditional boot either
publishes no such entry until its runtime is selected to serve, or evicts the entries it
published before the exit, so the ordinary path opens its own. Preserve configured modes and
`config_id` through both paths. The ordinary path performs its normal repairs and readiness
checks, and its own writes acquire the ordinary leases. A failed daemon start never authorizes a
local request that strict forwarding would refuse.

### Decision 3: account for each boot writer

Each row is part of the implementation and acceptance population. A no-create flag on the main
database alone is insufficient. Boot ends when the transport admits its first request. An effect
scheduled before that point belongs to boot, including one that completes asynchronously after
the server build returns.

| Surface                          | Conditional-boot disposition                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| -------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Database connection and identity | No CREATE, directory bootstrap, nonce initialization or journal-mode change. Keep physical identity, volume admission, path checks and existing WAL/SHM safety rules. Connection pragmas needed for reads must not execute schema or application writes.                                                                                                                                                                                                                                                                                                                                       |
| Core and pack schema             | Reader validation with the coverage stated above. No migration, idempotent DDL transaction or catalog omission. Missing/behind prerequisites take Decision 2's ordinary path.                                                                                                                                                                                                                                                                                                                                                                                                                  |
| Model registry                   | Reader check that each model in a runtime's persistent registration population (Decision 2) has its active registry row as Decision 2 defines it, in a registry whose definition matches Decision 2's reference and whose integrity check reads `ok` (Decision 2), in every writable runtime the host assembles. The registry DDL and upsert are prepared without stepping; none is executed at conditional boot. A missing row takes Decision 2's ordinary path.                                                                                                                              |
| Store schema gates and repairs   | Per-store schema gates, the notes sequence repair, sparse and text index preparation, and vector preparation keep their first-use behavior: conditional validation marks none of them ready, except a gate boot itself reaches (Decision 2). The first local request that needs one runs it with its ordinary writer, lease and audit, as request work; on a writable store the notes sequence repair still runs on the first notes acquisition, before any local cursor read. A factory, hook or mount that reaches one during conditional boot is a defect the acquisition observer reports. |
| Configured vectors and ANN files | Validate existing vector tables and columns through a reader, without forcing configured read-only mode or relying on a new instance's empty preparation cache. Do not warm a client ANN, register its checkpoint consumer, or create segment directories/checkpoint locks at boot. Path calculation is not evidence of file creation.                                                                                                                                                                                                                                                         |
| Events sidecar                   | A direct binding needs an existing-file, no-create validation route retaining configured writable mode, canonical-path cache policy and trust checks. Validate existing permissions against the same policy; required hardening takes Decision 2's ordinary path, so conditional boot does not chmod or admit insecure backing. The frozen-snapshot read-only opener, which rejects live-writer SHM, is not a substitute, and its guards must not be weakened. Socket-backed events retain their existing forwarding route. Missing direct storage follows Decision 2.                         |
| Attachment cutover and hydration | Read and require complete cutover, including the existing secondary-store legacy-attachment check. No GC-owner acquisition, staging, importing or finalization at conditional boot. Hydrator setup opens an existing blob root without creating it; Decision 2 gives a missing root its exit. Its construction cannot be assumed pure merely because cutover is complete.                                                                                                                                                                                                                      |
| Audit sink                       | Retain the configured sink and idempotent-audit capability preflight, using no-create validated backing. Do not call `with_read_only_audit_store` for a configured writable client: that drops the sink and changes request behavior. Missing backing follows Decision 2; incompatible capability remains an error.                                                                                                                                                                                                                                                                            |
| Diagnostic and attribution files | Keep their existing writes: the writer-timeout diagnostic sink (its directory resolves from `KHIVE_WRITER_TIMEOUT_SINK_DIR`, else `~/.khive/logs`, else `.khive-logs` beside the database; it is started at pool construction and writes from its own thread), the session WAL-pin beacons that ADR-091 Amendment 2 attributes with, the daemon rendezvous and recovery lock files, and the daemon launch log. They lie outside the configured stores, take no volume lease and hold no application or schema state. They are not disabled or deferred to obtain a zero count.                 |

SQLite's coordination with an existing live WAL/SHM retains its existing rules. The claim here is
no store creation, application/schema initialization or client volume-lease acquisition during
conditional boot, not zero operating-system writes. Lease counts do not prove filesystem purity;
the acceptance checks observe database contents, paths and permissions separately. Do not weaken
read-only snapshot, daemon-ownership or file-pinning guards to obtain a green count.

Requests local by design (`save_to`, request planning and bridge diagnostics) retain their normal
routes on the read-validated runtime. A local request that writes acquires its ordinary lease and
retains its audit obligations. A successful forward executes in the daemon and does not append a
second client dispatch audit. These request effects are outside the boot-only no-write claim.

### Acceptance: client attribution and controls

These are deterministic required witnesses, not measured performance results. All run through the
existing host validation queue; this Proposed text claims no native result.

1. A test-support-only observer at the khive-db volume-lease acquisition point counts attempts in
   the **client process**, across every configured store and boot phase, including identity,
   schema, model registration, vectors and a direct events sidecar. It must observe the real
   acquisition path, not a wrapper that a boot writer can bypass. An uninstalled observer fails
   the witness, and every run asserts that the test-only measurement bypass, which returns before
   the acquisition point, is off. The daemon's own acquisitions are separately attributed.
2. Use one persistent, already-current fixture with a persistent identity, registered configured
   models, configured vectors, complete attachment cutover, an existing blob root, audit backing,
   and a mounted nonempty schema plan with declared columns. With the condition true, eager boot
   plus one actual forwarded request records zero client acquisitions. With the **same fixture**
   and only the boot condition made false, ordinary boot records at least one. A normal forward
   must reach the daemon, not an injected response. Removing conditional selection, executing the
   writable pack plan or running model registration must make the zero-count witness fail;
   disabling the observer must also fail. A second run uses a store created by an earlier release
   and migrated through every supported schema version to this one, served by a daemon of this
   build: it too must take conditional boot with zero client acquisitions, so a registry or schema
   rule that admits only freshly created stores fails it. That fixture names the release and commit
   that created it, the sha256 of its archived image and the schema versions it migrated through,
   and the run keeps the migration and boot-selection records, so the control can be repeated.
3. Compare client boot before/after images independently: database identity and schema/version,
   the model registry, configured vector tables, sidecar and segment/checkpoint paths, attachment
   state, the blob root and relevant permissions. The after image is taken when the transport
   admits its first request, after every boot-scheduled asynchronous effect has signalled
   completion (the diagnostic sink's startup line, beacon registration). During this interval the
   fixture's daemon makes no store writes: its warmup and checkpoint tasks are held by
   test-support configuration until the client's after image is taken, no other client is
   attached, and the daemon's own acquisition count across the interval is asserted zero, so no
   daemon write is attributed to the client and none can hide a client write. Then release those
   tasks and send the forwarded request. The diagnostic and attribution paths of Decision 3 are
   listed expected differences. Images do not show a write undone before the after image, so
   test-support observers count in the client process across the same interval. The observer at
   the native database open counts opens carrying the create flag, which must read zero, and opens
   without it, which must read at least one per configured store, so the zero is not an
   uninstalled or bypassed observer. The observers at directory bootstrap, journal-mode change,
   nonce initialization and permission hardening count attempts and must read zero. Existing WAL/SHM safety checks and
   configured `config_id` must match ordinary boot. A forced read-only configuration must fail the
   identity/capability witness.
4. Exercise socket-only and PID-only rendezvous, unset/truthy/explicit-false strict values,
   absent rendezvous, `KHIVE_NO_DAEMON=1`, daemon role and in-memory main. Check Decision 1's
   selection and the strict request refusal contract, including the retained boot observation and
   stale rendezvous
   removal. A failed forward in strict mode must not execute the local mutation, including
   after an ordinary-boot exit.
   Specifically, begin with a stale rendezvous and a missing configured store, force daemon-first
   startup to remove the stale paths and launch a daemon that never binds, then take ordinary
   eager boot with strict unset. Both paths must be absent **before the first request**. That
   first normal write must refuse with the strict-forward error, without local mutation or its
   dispatch audit. Dropping the retained boot observation must make this witness fail. Check
   stale socket and stale PID separately; a store-present control that removes paths only after
   request entry does not exercise this earlier lifetime. Pair it with the existing no-boot-
   rendezvous and explicit-false behavior, and a second independent host that cannot inherit the
   first host's observation.
5. Independently remove a configured store, the identity table, a declared pack column, a vector
   table, a configured model's registry row, a boot-reached store schema object, the default blob
   root, an explicitly configured blob root or a direct sidecar, and provide behind-schema and
   incomplete-cutover fixtures. Check the specified daemon-first or ordinary-boot exit, without
   serving the provisional runtime; after a blob-root exit hydration is available, and for an
   incomplete cutover the selected exit is asserted separately from whether the ordinary repair
   succeeds. Test a missing and an incompatible declared pack column separately and together; the
   pair must refuse. Identity swap, an identity table without its row, a malformed identity,
   volume/path-trust refusal, malformed blob configuration and incompatible schema remain
   must-refuse controls, asserted before any repair write, not repair opportunities. A sidecar
   requiring permission hardening must leave the conditional path before exposing its sink. A
   later store's exit after a direct sidecar was opened must leave the serving runtime on the
   ordinary path's events backend, not the provisional one. The missing-store case runs through
   the production startup entrypoint, which holds the boot recovery lock, with a real daemon child
   that needs that lock to start: the child must become ready and the client must validate again
   after it; a direct server-build call lacks the outer lock and does not exercise this ordering.
   Include a mounted pack in these checks; built-in-only coverage is insufficient. The removed model
   row belongs to a built-in model in a writable runtime's persistent registration population. A
   second arm removes the row of a configured built-in alias whose provider a pack replaces before
   serving with one of matching dimensions; it too takes ordinary eager boot and registers the row.
   Two controls keep conditional boot: a read-only per-pack runtime whose configured model has no
   row, and a configured engine whose name parses as no built-in model alias, served by a provider a
   pack registers. Neither takes an ordinary-boot exit or gains a registry row, and each configured
   provider is bound as under ordinary boot. Registry states other than a missing row each take
   ordinary eager boot with its result as today: a row with the model's key in `superseded` status,
   with no other active row for its engine name, is reactivated; an active row with the model's key
   and no activation time has it set; a missing registry index is created. An active row for the
   model's engine name under another key, with no row carrying the model's key, and a registry table
   without its `output_dim` column with the model's active row and both indexes intact, each keep
   ordinary boot's refusal of the insert, with the host result Decision 2 names: an error on a
   single-backend host, a logged warning on a multi-backend host that goes on to serve. An events
   table with neither operation attribution column takes ordinary eager boot and is upgraded; one
   with exactly one keeps ordinary boot's refusal; one whose two attribution columns exist only as
   generated virtual columns takes ordinary eager boot and keeps the refusal of the upgrade; a
   missing events index takes ordinary eager boot and is created. Further registry arms each keep
   the model's active row, its activation time and the names of all three registry objects, and each
   takes ordinary eager boot with ordinary boot's result: a table without the `UNIQUE` on
   `canonical_key`, whose upsert SQLite refuses when compiling it; an added column that requires a
   value and has no default, with the existing row given one, whose insert is refused; an added
   `CHECK` that the existing row satisfies and the next registration's creation time does not; an
   added trigger that writes another table on update, one that refuses the update, and one that
   writes another table before an insert, which SQLite runs before the upsert resolves its conflict;
   an added index, and an existing index redefined under its own name; a whitespace change inside a
   string literal of the status check; and an active row whose canonical key is stored as TEXT with
   the same characters, whose insert is refused on the one-active-per-engine index. A further
   registry arm keeps the definition exact and the model's active row with its activation time set,
   and stores the text `8` in that row's INTEGER `output_dim` (an image written under a TEXT
   declaration of equal length whose declaration is then changed, which reads normally and which an
   integrity check reports); it takes ordinary eager boot, whose upsert succeeds and stores the
   integer 8 there. Another stores the real number 8.0 in that `output_dim` (written under a
   declaration of equal length with no affinity, `BLOB`, which is then changed), which reads
   normally and which the registry-scoped integrity check reports as `ok`; it too takes ordinary
   eager boot, whose upsert succeeds and stores the integer 8. Another keeps the definition, the
   model's active row and its values exact and removes that row's entry from the one-active index
   (an image edit), which the registry-scoped integrity check reports and a quick check does not; it
   takes ordinary eager boot, whose upsert fails as a malformed image, with the host result Decision
   2 names. Another adds an empty table elsewhere whose column references the registry's `status`
   column, which leaves every registry object and the row exact; it takes ordinary eager boot, whose
   upsert SQLite refuses when compiling it with foreign key enforcement on. Further Events arms take
   ordinary eager boot with ordinary boot's refusal: an
   `events` table that is an FTS5 virtual table carrying every expected column, with every expected
   index name placed on another table, and the same for `event_observations` alone. Controls keep
   conditional boot: a registry created by the V1 schema, one created by the registry DDL, and one
   whose definition differs from the DDL only in whitespace and comments between tokens; an events
   table whose attribution columns the upgrade added; and an expected events index name that exists
   on another ordinary table. A test-support observer counts the prepared check statements that are
   stepped against a configured store and must read zero. A mutation that classifies any must-exit
   state by a reader without taking ordinary boot must fail its arm, and so must a reader that
   checks registry columns, indexes or other properties by list instead of comparing the definition,
   compares only the objects the DDL names, skips preparation, prepares the registry statements
   without foreign key enforcement (the incoming foreign key arm), prepares only the first Events
   statement, steps a prepared statement, compares the definition but not the stored values of the
   model's row (the real `output_dim` arm), replaces the registry-scoped integrity check with a
   quick check or none (the missing index entry arm), or counts hidden or generated columns as
   present, as an
   unfiltered `pragma_table_xinfo` listing does (the generated-column arm).
6. After validated conditional boot, exercise local planning/diagnostics and a local `save_to`
   request that writes and audits. Require its real lease and audit record. Require exactly the
   existing daemon-side dispatch audit for a forwarded write and no duplicate client dispatch
   audit. Dropping the configured sink must fail. A test-support event store whose
   idempotent-audit capability reads false, bound through the conditional host build, must refuse
   before serving with the existing incompatible-event-store error, beside the healthy-backing
   success above; removing the capability preflight must make that refusal witness fail.
   A first-use notes witness runs on the same conditional host. The fixture is current in every
   respect Decision 2 checks and holds two live notes in one namespace: one with its `notes_seq`
   row and one without it. After conditional boot, with the client's boot lease count and the
   backend's notes sequence repair count both zero, a local `save_to` request carrying
   `list(kind="note", after=<the sequenced note's id>)` must run the repair on its notes
   acquisition, through the ordinary writer and its lease, before the sequence-cursor query reads,
   and the saved result must contain the unsequenced note. `save_to` keeps the request local under
   the preserved forwarding policy, and the `after` argument selects the cursor read. The same
   request after ordinary boot is the control. Marking the repair as already run during
   conditional construction alone must make this witness fail.

The acceptance gate needs executed baseline and independent mutation controls, with compilation
or setup failures distinguished from the intended assertion failures. In particular, removal of
strict fallback refusal must fail a must-refuse request witness. Neither a named test nor a
zero count from an inactive observer is a passing result.

### Scope and acceptance status

This is the narrower property left open by Amendment 14, not a latency or memory optimization
claim. No historical timings are extrapolated. Store ownership, daemon protocol, request retry
semantics, timeouts, configured modes, full discovery and mount visibility are unchanged.

The docs-only proposal carries no implementation or executed acceptance. Independent spec review
and approval through the designated specification gate are required before dependent
implementation merges; a merged Proposed text does not itself grant that acceptance.

## Amendment 16 (2026-10-25): declared store maintenance with responsive stdio bridges

**Status**: Proposed. **Disposition**: amends Decision §2's fallback rules, and the no-socket outcomes of Amendment
14 Decision 2, for the time a declared maintenance epoch is closed, for #6063. The base ADR remains Accepted. This
amendment neither accepts Amendment 15 nor restores the lazy registry that Amendment 14 withdrew, and it adds no pack
metadata API and no mounted-catalog contract. Dependent implementation does not merge before this amendment's
acceptance gate.

**Origin**: the measurement of stdio bridges across the store-migrating upgrade of 2026-10-24, recorded in #6063,
and the interim operator procedure decided after it. It is an engineering decision drawn from that measurement; no
product-owner instruction prompted it.

### Context

An upgrade whose new build migrates the stores cannot use the in-place path a protocol bump uses. The daemon is
stopped for a snapshot and the migration. A request that reaches a running stdio bridge in that window takes the
no-socket path, where it may spawn a daemon or, with strict mode unset and no rendezvous on disk, dispatch locally
against the stores being migrated. The supervisor marker (ADR-185 Amendment 1) already holds a client for up to three
restart intervals with no spawn and no local dispatch (measured in #6063). Every running bridge, however, opens the
configured stores read-write at boot and keeps them open while it only forwards. A procedure that requires no process
to hold a store file therefore stops every bridge, and an MCP client that does not restart its stdio server needs a
manual reconnect.

Four facts at the inspected revision shape the decision:

1. A bridge runs no WAL checkpoint task, schedule ticker or channel loop; those start only in the daemon role
   (`start_host_background_tasks` in `crates/khive-mcp/src/serve/startup.rs`, the checkpoint task in
   `crates/khive-runtime/src/daemon/boot_guard.rs`). Its background work beside the stores is the session sweep
   (`run_session_sweep_task` in `crates/khive-db/src/checkpoint/lifecycle.rs`), which never checkpoints and opens no
   SQLite transaction but writes the process's WAL-pin beacon file beside the database on every tick. A bridge also
   holds the events database open writable in direct mode, because only daemon hosts switch the event plane to
   socket forwarding (`enable_events_forwarding_for_daemon` in `crates/khive-mcp/src/serve/runtime_config.rs`); its
   audit batch appends only rows that local dispatch submits (`crates/khive-runtime/src/audit_batch.rs`).
2. A bridge checks its executable's identity on each `request` call before dispatch
   (`crates/khive-mcp/src/server/bridge.rs`); `tools/list` does not run the check. A bridge whose executable was
   replaced returns a retryable error and re-execs in place after that response, keeping its PID and stdio
   descriptors, on every install (`arm_executable_self_heal` in `crates/khive-mcp/src/daemon/self_heal.rs`). Only a
   protocol mismatch seen by an already resumed bridge takes the drain-and-exit path.
3. A re-exec'd or newly started bridge runs the ordinary boot before it serves: it opens the configured backends,
   prepares the core schema, runs the attachment cutover, applies pack schema plans in write transactions and
   registers the configured embedding models (`crates/khive-mcp/src/serve/runtime_config.rs`,
   `crates/khive-mcp/src/server/pack_setup.rs`, `crates/khive-runtime/src/config.rs`). Boot is neither
   migration-free nor write-free, so a boot inside the window would migrate or write stores mid-procedure, and
   bridges that refresh together after release serialize on those writes.
4. A connection pool closes its writable connection last so that SQLite can checkpoint on close
   (`impl Drop for ConnectionPool` in `crates/khive-db/src/pool.rs`). A process that holds the last open connection to
   a store rewrites the database file and removes the `-wal` when it exits.

The numbered SQL migrations at the inspected revision change no journal mode or page size and run no VACUUM or
checkpoint. The code-map store's DELETE-journal transition (`crates/khive-db/src/code_map_vfs/transition.rs`) is a
separate policy and is outside this procedure.

### Decision

1. A declared maintenance epoch is a hard admission fence over an explicit canonical physical-store set, including
   configured secondary databases and derived event stores. It is distinct from the supervisor boot marker and its
   elapsed-grace behavior. Expiry, owner death, `KHIVE_NO_DAEMON`, non-strict configuration and absence of a
   rendezvous never authorize local store access or daemon spawning while an epoch is closed. The store set is
   derived from the resolved configuration: the main database, each configured backend database and the derived
   events database, each by canonical path. The declaration is one record per store set, kept in the directory of
   the main database, owned by the user that owns the stores, created owner-only and replaced atomically. It carries
   an opaque epoch; its phase (`closing`, `quiescent`, `migrating`, `ready`, or a failed state that stays closed);
   the store set; the target installation's identity and configuration; owner evidence, which is the coordinator's
   process ID, start time and executable identity; the digest of the epoch capability's secret (Decision 2); and
   bounded phase deadlines.
   The coordinator writes `ready` only at release (procedure step 11), after the target's readiness has been verified
   while the record still reads `migrating`. `ready` is the only phase that opens admission, and that record, kept at
   the same location until the next epoch's declaration replaces it, is the release receipt. The coordinator is the single
   writer: it publishes the record, advances its phases and writes the receipt, and the owner evidence names it. A
   declaration or receipt from any other writer, or one a participant cannot attribute to its epoch's coordinator, is
   refused: it neither opens admission nor releases a bridge, and the participant stays closed and reports it. A
   participant likewise treats as a closed epoch, never as an absent one, a record it cannot parse, one whose file
   owner is not the user that owns the stores, one that grants access to any other user, and one in a directory that
   any other user can write. A participant checks the owner evidence against the epoch and the store set it carries;
   it cannot tell one process of the stores' owner from another, so attribution among that user's processes is
   cooperative. The trust boundary is the user that owns the stores: a process running as that user can already
   open the stores directly.
2. Publication closes admission to new boot and local store work. Work admitted beforehand must settle under tracked
   activity leases before the coordinator certifies quiescence. Leases cover actual asynchronous completion and
   non-SQL store effects, not the idle lifetime of a runtime or SQLite connection. A bridge's session sweep, which
   writes its WAL-pin beacon, is either taken under a lease or stopped while the epoch is closed; it is not exempted
   as read-only. Each participant takes a per-store shared admission lease before boot or local store work and reads
   the declaration while holding it, so a check cannot be separated from the open it admits. The coordinator
   publishes `closing`, waits for the leases admitted earlier to drain, then takes the set exclusively in canonical
   path order. Leases have operating-system lifetime semantics, so a process that dies releases them. A lock held
   through an open file description is shared with a child forked from its holder, so each lease descriptor is
   created close-on-exec and a process holding a lease starts other processes only by spawning a new image: a child
   carries the descriptor only until its own exec, so a lease ends with its holder, or at the latest when a child the
   holder was starting at that moment execs.
   The coordinator owns exclusive maintenance admission for the entire
   store set through snapshot, migration and readiness; only its capability for this epoch and store set passes the
   closed fence, and there is no general override. The coordinator starts the target daemon as its delegate and hands
   it that capability at launch on an inherited descriptor, never in argv, which process listings show, or in the
   environment, which every child inherits. The coordinator creates the descriptor close-on-exec and clears that flag
   only in the delegate's child, after the child is created and before its exec. The coordinator's own descriptor
   never loses the flag, so a process started from another coordinator thread at the same moment does not receive it,
   and neither does any other process the coordinator starts. The delegate marks it
   close-on-exec again before it starts any thread or process, and it starts processes only by spawning a new image,
   so a process the delegate starts does not inherit it; the delegate hands it on, the same way, to the events daemon
   it supervises and to no other process. The capability is an owner-only regular file that the coordinator creates
   exclusively in the declaration's directory, opens and unlinks at once, before it publishes the declaration, so no
   path names it and only a process holding the descriptor can read it. It holds the epoch, the canonical store
   set, the recorded target identity and a random secret of at least 128 bits, and the declaration records the
   secret's digest. The
   delegate learns the descriptor's number from an option value, which names a descriptor and grants nothing. The
   fence reads the capability through the descriptor from offset zero, so a shared file offset does not matter, and
   admits only when the descriptor refers to an unlinked regular file owned by the stores' owner and readable by no
   other user, the file parses, its epoch, store set and target identity equal the current declaration's, and its
   secret's digest equals the one the declaration records. Anything else is no capability: a number that names no
   open descriptor or names anything else, a file that still has a path, and a descriptor kept from an earlier
   epoch, since each epoch mints a new secret. The coordinator closes its own descriptor at release. Under the
   coordinator's exclusive hold the fence admits the delegate's boot,
   migration and readiness checks, and that events daemon, and no other process. The delegate and that events daemon
   take no admission lease while the epoch is closed: the coordinator's exclusive hold covers their store work and the
   capability is their admission, so neither waits on the hold. The capability ends at release. It
   keeps cooperating processes that were not started as the delegate out of the closed fence; it is not a defense
   against a hostile process of the same user, which on Linux and macOS alike can open the stores directly and which
   the trust boundary in Decision 1 already excludes. A marker check without synchronized admission is insufficient.
   Successful migration invalidates activity admission from old runtime generations, background work included, until
   the process has refreshed; a proved pre-mutation abort may keep the predecessor generation.
3. Existing stdio bridges retain PID and transport and return structured retryable `maintenance_in_progress` errors
   carrying the epoch and phase within caller budgets. They do not execute local fallback or local `save_to`, refresh
   a mounted catalog from stores, enter degraded bootstrap, or spawn a daemon. Process-only diagnostics may continue,
   and `tools/list` answers from the catalog already in memory. While an epoch is closed a bridge returns the
   maintenance error before its executable-identity check and arms no exec, so an installation inside the window
   cannot start a boot of the new image before release. Cold clients wait before opening any configured store; there
   is no fabricated temporary tool catalog.
4. This procedure permits existing idle connections only for a certified same-file WAL migration. Certification
   covers the exact migration range, pack plans, boot repairs and associated files. Changing journal mode or page
   size, replacing or unlinking a live database, `-wal` or `-shm`, and offline restore are excluded. The snapshot is a
   transaction-quiescent, complete store-set copy plus a restore proof with the previous binary; an online backup of
   one database is not a substitute for that proof. The coordinator opens a connection on every store in the set
   before it certifies quiescence, and so before the old daemons stop. These are plain connections that run no
   schema preparation. It holds them at least through the end of the copy and closes them at release, so that no
   process exiting during the copy, the
   stopped daemons included, can run a close-time checkpoint. The copy proves each database, `-wal` and `-shm`
   unchanged across it. No write transaction runs and the coordinator's idle connections hold no read transaction,
   so no process changes a `-shm` during the copy, and each copied `-shm` is the WAL index of exactly the copied
   `-wal`. Every copy, the vector index copies and the writable restore copy included, is created owner-only, like
   the stores. The copy is then frozen
   read-only, as read-only inspection requires: it refuses a writable `-shm`, and it refuses a non-empty `-wal` with
   no `-shm` beside it, because SQLite would then either omit committed WAL frames or create a new index
   (`read_only_wal_open_target_for_path` in `crates/khive-db/src/pool/connection_setup.rs`). The restore proof runs
   on a writable copy of it made without the `-shm`, so the previous binary rebuilds the WAL index from the `-wal`
   rather than trusting the copied one. An idle
   connection holds no WAL read mark, so it does not block the target daemon's open-time checkpoint under a
   configured WAL ceiling; a read transaction does, and it is covered by a lease.
5. The declaration precedes atomic executable installation. Release requires the declared target daemon to prove
   schema, configuration and protocol readiness, and leaves the `ready` receipt of Decision 1, which records the
   completed epoch and the target. Before any
   stale-runtime store-sensitive work, a bridge observes that receipt and validates the installed executable's
   identity without the ordinary throttle. Identity is the existing device-and-inode identity (modification time where no
   inode is reported) of the path the bridge started from (`BridgeExecutable` in `crates/khive-mcp/src/daemon/executable.rs`); the receipt records the target's
   identity at that path, the bridge execs only when the installed path matches it, and the exec target is always
   that path, never a value carried in a message. A changed image uses the existing response-triggered in-place exec and
   resumed handshake mode. No asynchronous park or resume exec is introduced, and the ordinary protocol-mismatch loop
   breaker remains unchanged. A `tools/list` answered before the refresh carries the previous image's tool list and
   verb description, as a client that listed tools before any install already does. Failure to validate or exec the target inhibits work; it
   does not authorize stale local dispatch.
6. Declaration scope is independent of `HOME` and socket. All participating boot, local and CLI paths enforce it
   before the recovery lock and before any store opens. The procedure starts only from an installed executable that
   already participates; an installation older than this amendment is first upgraded by the current install path,
   outside these guarantees. Non-participating executables, those older than this amendment that do not read the
   declaration, and external holders are inventoried before publication (step 1 of the procedure order): each such
   process is stopped by PID or upgraded, and the coordinator does not publish while one remains. Participating
   processes on the previous build are not in that inventory: existing bridges keep running under Decision 3, and the
   old daemon and events daemon stop at step 5. A non-participant that holds a store file at the holder check (step 6) ends the epoch before any
   snapshot, and the copy proves the database and `-wal` unchanged across it. A non-participating process that starts
   during the window is not controlled by an advisory protocol; that limit is stated, not closed. Existing daemon
   singleton claims remain distinct.
7. A failed drain aborts before migration. A failed or abandoned epoch remains closed until an explicit, proved
   recovery action. After a pre-mutation abort, the holder check at step 6 included, that action may be resuming the
   validated old service; after mutation, recovery is forward
   repair or the existing offline restore of the paired stores and binary. Transparent PID-preserving offline rollback
   is not promised.
8. With no declaration and no migrating upgrade, current boot, catalogs, fallback, `save_to`, in-memory operation and
   daemon recovery semantics remain unchanged. The existing risk that response-triggered self-heal loses a pipelined
   stdin frame is documented separately; this amendment does not claim a lossless transport handoff.

### Procedure order

1. Inventory non-participating executables and external holders of the store set (Decision 6). Stop each such process
   by PID or upgrade it, and do not continue while one remains. Participating bridges and the old daemons on the
   previous build are not stopped here.
2. Create the epoch capability (Decision 2), then publish the declaration for the store set, carrying the
   capability's digest, in its `closing` phase.
3. Drill a cold client: it waits and spawns nothing.
4. Open the coordinator's connection on every store in the set, then drain activity leases until quiescence is
   certified, and write `quiescent`.
5. Stop the old daemon and events daemon by PID. Bridges stay running, and the coordinator's open connections keep
   either exit from running a close-time checkpoint.
6. If any process outside the participants holds a store file, end the epoch in a failed state, which stays closed
   until an explicit, proved recovery action (Decision 7); take no snapshot and run no migration.
7. Copy every database, `-wal` and `-shm` and every vector index directory of the set, prove the database, `-wal`
   and `-shm` files unchanged across the copy, and freeze the copy read-only.
8. Prove that a writable copy of the snapshot, made without its `-shm`, restores with the previous binary.
9. Install the new executable atomically.
10. Write `migrating`, then start the target daemon as the coordinator's delegate, holding the epoch capability. It migrates the configured
    backends in place and starts the events daemon, which prepares the events schema before it accepts traffic.
    Verify readiness while the record still reads `migrating`.
11. Release by writing the `ready` receipt, then close the coordinator's connections.

### Acceptance

These are obligations for the implementation, run in Linux CI with fixture binaries, a temporary `HOME` and
temporary stores, explicit test barriers instead of sleeps, and process identity read from `/proc/<pid>/exe`. Each
fails on the code at the inspected revision unless stated otherwise.

- **Responsive bridge across the window, then two installs.** With a declaration closed and the daemon stopped, a
  read, a mutating call and a `save_to` call through one live bridge each return the maintenance error within budget,
  with no local result, no saved file and no new daemon PID; user data in the stores is unchanged. After release the
  next call succeeds on the new image under the same bridge PID and pipes. A second install on the same PID behaves
  the same way. An executable installed while the epoch is closed starts no exec before release. The
  repeated-identity exec alone works today and is a control.
- **Check-then-open races.** A cold boot paused just before its first SQLite open, and a warm local or `save_to` call
  paused just before store-work admission, do not cross a declaration published while they are paused. The
  coordinator cannot certify quiescence while a lease admitted earlier is outstanding.
- **Every entry point.** Two backends plus a derived events store; a second bridge with a different socket and `HOME`
  over the same physical stores; local `exec`; direct events and admin paths. Every store-dependent entry waits or
  refuses; metadata-only help stays available; a cold client holds no configured store descriptor before release.
- **Declaration record.** A record that cannot be parsed, one that is not owner-only, one owned by another user, one
  in a directory another user can write, and one written by a process other than the epoch's coordinator each keep
  every entry closed and release no bridge. A `ready` receipt whose
  target identity does not match the installed path leaves the bridge inhibited, with no exec. A record that still
  reads `migrating` after the target's readiness checks pass keeps every entry closed and releases no bridge, and the
  `ready` record written at release opens admission. During migration the
  delegate target daemon and its events daemon are admitted without taking an admission lease. A process of the same image started without the epoch
  capability is refused, and so is a child the delegate starts other than its events daemon, including one started
  at the earliest point the delegate's startup allows; a process the coordinator starts other than the delegate holds
  no capability descriptor, including one started from another coordinator thread while the delegate spawns; the
  capability appears in neither the delegate's argv nor its environment. While the epoch is closed the fence refuses
  a capability descriptor kept from an earlier epoch, a capability file that still has a path, one another user can
  read, and a file carrying the current epoch, store set and target identity with a secret whose digest the
  declaration does not record.
- **Old images and holders.** A non-participating process found by the inventory (step 1) keeps the coordinator from
  publishing until it is stopped or upgraded, a bridge running the previous participating build keeps its PID through
  the window, and a fixture non-participant holding a store file at the holder check
  (step 6) ends the epoch with no snapshot and no migration.
- **Drain means settlement.** Read and write transactions, and a cancelled request whose spawned continuation is
  still running, keep the coordinator in the closing phase until each settles; a deadline fails entry with no
  snapshot and no migration, while idle descriptors stay open.
- **Snapshot and migration classes.** With idle bridge connections held, the copy restores with integrity and
  expected counts, and the source migrates without closing those descriptors. A fixture migration that changes the
  journal mode, and one that replaces a store file, is refused before the source is touched. A fixture client that
  exits during the copy holding what would otherwise be the last connection leaves the copied database and `-wal`
  consistent. A fixture daemon that is the only other holder, stopped after quiescence, leaves the database and
  `-wal` unchanged. The frozen copy passes read-only inspection and shows the rows committed only in its `-wal`, a
  `-shm` that changes during the copy fails the copy, the writable restore copy carries no copied `-shm`, and every
  copy is owner-only. With a WAL ceiling configured and the WAL over it, the
  target daemon's open-time checkpoint succeeds while idle bridge descriptors are held.
- **Failure, owner death and deadline.** A coordinator killed before mutation, or after one migration step, a request
  budget that expires, and a target whose stat, exec or readiness fails, each leave the bridge alive and returning a
  named retryable inhibition, with no grace-time fallback and no spawn. A lease holder killed while a child it
  started is still running leaves no lease held: the child carries no lease descriptor, and the coordinator
  certifies quiescence without waiting on that child.
- **Release receipt and throttle.** A resumed bridge idle for the whole window, released inside the ordinary identity
  throttle interval, checks the target on its first `request` call and refreshes by the identity exec, not by
  drain-and-exit, with no new MCP `initialize`. A `tools/list` before that call returns without an error and without
  a store read.
- **Refresh after release.** Several resumed bridges released together each complete the refresh, and the elapsed
  time and writer waits of their boots are recorded. This is a measurement, not a test that fails today.

Compatibility controls, which need not fail today: with no declaration, forwarding, strictness configuration,
permitted no-socket fallback, local `save_to` manifests, in-memory operation, configuration identity, catalog
filtering and protocol-mismatch loop breaking are unchanged. A deterministic two-frame pipelining probe runs against
the base and the candidate; any loss on the base is recorded, and a loss only on the candidate blocks the change.

### Alternatives considered

1. **Polling only, with no activity barrier.** A process can pass its check just before the declaration and then open
   or migrate a store, and work submitted earlier can still be running. It cannot justify the copy boundary.
2. **A permanently store-free forwarding bridge.** Static handler metadata does not describe mounted packs, replay
   trust, `save_to` or local fallback. Removing fallback changes behavior without a declaration; lazy fallback brings
   back the descriptors this design would avoid. The lazy registry was withdrawn in Amendment 14.
3. **A client-lifetime fence with an exec into a parked host and a resume exec.** It reaches zero descriptors, which
   an in-place WAL migration does not need, at the cost of transport buffering, child cleanup, a parked catalog and two
   execs per window.
4. **Stopping every bridge with SIGSTOP.** Open descriptors are tolerated, but a stopped bridge cannot answer with a
   budgeted error, may be frozen inside a transaction, and does not hold back a client that starts afterwards.

### Out of scope

A lossless stdin handoff across exec; PID-preserving offline rollback; migrations outside the certified same-file
WAL class; control over processes that do not participate; any change to Amendment 15.

### Scope and acceptance status

The docs-only proposal carries no implementation or executed acceptance. Before use, the snapshot step needs a
measured restore on the target filesystem with idle descriptors held. Independent spec review and approval through
the designated specification gate are required before dependent implementation merges; a merged Proposed text does
not itself grant that acceptance.
