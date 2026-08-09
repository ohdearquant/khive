# Daemon lifecycle — probe, recovery, forwarding (ADR-049)

`daemon` (`src/daemon.rs`) is the client side of the warm-daemon protocol: it
probes an existing `khived` daemon, spawns one if absent, recovers from a
stale/dead one, and forwards request frames over the daemon socket. This
document is the extended rationale for the concurrency and safety properties
that the inline doc comments summarize.

## Recoverer lock — mutual exclusion across concurrent recoverers (#838)

`kill_and_respawn` kills a stale daemon and spawns a fresh one. It implements
**double-checked recovery**: a cheap bounded `probe_only` frame is sent under
the shared boot/recovery lock first, letting a client that finds the daemon
obviously alive return `Skipped` immediately without ever touching the
recoverer lock.

Before #838, a bare `Dead` reading on the initial probe fell straight into
`confirm_genuinely_dead` → kill → spawn with no linearization point across
concurrent recoverers — two clients racing from a genuinely dead daemon could
both classify `Dead` and both spawn a replacement. The fix acquires
`try_acquire_recoverer_lock_until` — a SEPARATE lock file from the daemon's
own boot lock — before `confirm_genuinely_dead` runs, and holds it through
kill + spawn. This makes recovery mutually exclusive across recoverers
without risking a deadlock against a booting daemon: the daemon itself never
acquires this lock, only `kill_and_respawn` does. A bounded, deadline-aware
acquisition (`RECOVERER_LOCK_TIMEOUT_MS` = 16000, generous enough to cover a
peer's full worst-case critical section) is used instead of an unbounded
`flock` so a second recoverer never blocks forever on a wedged first one.

A second lock file (rather than reusing the boot lock for the whole span) is
required because the daemon acquires the boot lock as the very first thing
it does on boot (`acquire_daemon_boot_guard`, see `kkernel::main`) — a client
holding that SAME lock across confirm-through-spawn would deadlock every
recovery attempt against the very child it just spawned and is waiting to
observe. `confirm_genuinely_dead` still uses the boot lock internally
(bounded, per-round) purely to detect quiescence; it never holds it across
the whole function. The recoverer lock is orthogonal — it excludes peer
recoverers from each other, not from the daemon.

Outcomes: `Alive`/`Timeout` (initial or confirmed) → `Skipped`, no kill
(NEVER-KILL-SLOW: a timed-out probe means the daemon may be alive but busy,
not dead). `LockContended` (confirm rounds could not establish quiescence) or
the recoverer lock itself timing out → `Uncertain`, no kill — same safe
behavior as `Skipped` but reported distinctly so it is never conflated with a
positive "confirmed alive" result. `Dead` (confirmed, recoverer lock held) →
kill + spawn → `Spawned`.

The test-only `RECOVERY_RACE_BARRIER` forces eight recoverers to reach the
classification-complete point at the same instant. Without it, normal Tokio
scheduling can let one recoverer finish before the others observe anything,
and a nominally parallel test can pass without exercising the lock race.

The recovery launcher is an explicit seam. Production closures still launch
`current_exe() mcp --daemon` and return the owned child handle unchanged. The
shared `daemon/test_harness.rs` fixture instead launches the real
`run_daemon` server in-process on a multi-thread Tokio runtime, so the test can
observe the server-side ownership fence. Its stable oracle is one responsive
daemon, one socket/PID rendezvous, and one successful `stats()` exchange after
quiescence. It deliberately does not require exactly one launch attempt: the
client releases its recovery lock before a launched daemon binds, so losing
attempts are legal as long as the server fence converges to one owner.
Because in-process candidates share a PID, the harness uses an explicit
fault-injection entry point that lets a responsive same-PID incumbent win;
ordinary startup retains its PID-reuse-safe behavior. A losing candidate still
cancels the runtime's process-wide component token, so this fixture deliberately
uses a component-free dispatcher and makes no component-lifecycle claim.

The companion eight-client ParseFailure test closes every connection only
after all real frames have been read. Every client must return the stable
ambiguous-forward error, while kill/spawn counters remain zero and the server
observes no follow-up connection. `scripts/ci.sh daemon-recovery-flake` runs
the paired scenarios 25 times on each CI operating system.

## `confirm_genuinely_dead` — closing the fork-to-flock gap (#758)

`spawn_daemon()` is fire-and-forget: `cmd.spawn()` returns as soon as the
child process exists, well before that child reaches its own
`acquire_daemon_boot_guard()` call. A bare identity probe taken in that gap
sees `NoSocket` and is classified `Dead` even though a replacement daemon is
legitimately on its way up. `confirm_genuinely_dead` retries
`quiesce_then_probe_identity` up to `DEAD_CONFIRM_ROUNDS` times, paced by
`DEAD_CONFIRM_POLL_MS`, and returns as soon as a peer's boot is observed
completing (`Alive`) or going slow (`Timeout`, NEVER-KILL-SLOW). Only
`Dead` once every round agrees.

`quiesce_then_probe_identity` blocks until no concurrent boot holds the
shared boot/recovery lock (bounded by `BOOT_QUIESCENCE_LOCK_TIMEOUT_MS` =
500ms), then re-probes daemon identity — successfully reacquiring-then-
dropping the lock proves neither a peer's kill+spawn nor a daemon's own cold
boot is currently mid-critical-section. Before #838 this used an unbounded
blocking `flock`, so `DEAD_CONFIRM_ROUNDS` bounded probe _count_ but not
elapsed _time_ — a wedged lock holder blocked recovery forever. A
deadline-elapsed or otherwise-failed acquisition returns the distinct
`ProbeOutcome::LockContended` rather than collapsing into `Timeout` (which
means something different: "the daemon itself answered slowly").

## Strict-mode fallback accounting (D2-R1/D2-R3, #947)

`is_daemon_strict_mode` (`KHIVE_DAEMON_STRICT=1`) elevates `Illegitimate`-
severity fallbacks (`ConfigMismatch`, `NamespaceMismatch`) from a WARN to an
error-level structured event plus `FALLBACK_STRICT_VIOLATIONS` (D2-R1), and
independently, `fallback_or_reject` rejects the request outright instead of
letting it complete through local dispatch. Together these make an
illegitimate mismatch impossible to miss AND make "strict mode active" a
sound proof that no request in the window was served off the local fallback
path — the daemon-engagement proof in Benchmark SPEC Amendment 1 §3 depends
on this. Every `FallbackReason` is rejected under strict mode, not just the
`Illegitimate` tier — that tier only governs the WARN vs ERROR log level
inside `record_fallback`, an orthogonal concern.

No hosted-vs-local auto-detection signal exists in this codebase; strict
mode is a plain opt-in, default OFF (matching `is_strict_actor_mode`'s
`KHIVE_REQUIRE_ATTRIBUTED_ACTOR` shape) — the hosted/fleet image sets
`KHIVE_DAEMON_STRICT=1` explicitly in its own deployment environment.

`fallback_total()` derives its total by summing the five per-reason counters
on read rather than tracking a separate atomic, so total == sum-of-reasons is
a structural invariant instead of a timing-dependent one (two independent
`fetch_add`s could otherwise be observed momentarily out of sync).

The `daemon_fallback` event renders client and daemon configuration identifiers
as stable, full SHA-256 identifiers rather than the path- and topology-bearing
fingerprints used for the equality check. A configuration mismatch also carries
`config_mismatch_field`, naming the first differing fingerprint field without
emitting either field value. Its ordered vocabulary follows the production fingerprint:
`packs`, `db`, `embed`, `extra`, `fresh_tail`, `backend`, `outbound`, `git_write`,
`backends`, then `pack_backends`. The wire equality check and fallback decision still use
the original full fingerprints.

The `khive_strict_daemon_fallback` marker on a strict-fallback rejection's
`McpError` (#947) lets `request()` in `server.rs` distinguish "the daemon was
never reached and strict mode rejected the fallback" from every other
daemon-forward `McpError` (protocol mismatch, oversized frame, ambiguous
post-write outcome), which stay RPC-level errors.

## `trigger_bridge_self_heal` — concurrency accepted-risk note (#714)

Called from both `forward_or_spawn`'s `ProtocolMismatch` arms (first-attempt
and post-recovery-retry). If the bridge is mid-flight on more than one
outstanding client request when the mismatch fires, only the request that
triggered this arm gets the ambiguous-error-then-resume treatment; any other
in-flight request loses its response the same way it would if the process
crashed — a pre-existing risk, not introduced by this change.
`fire_pending_self_heal` fires on the next successful flush of _any_
message, not specifically the mismatch response's own flush — on this
bridge's dominant single-request-at-a-time usage those are the same event,
but a genuinely concurrent second in-flight request could in principle flush
first. Strictly better than the pre-fix timer (which could fire before _any_
flush completed), and the same class of pre-existing risk, not a new one.

`SelfHealOnFlushTransport` wraps the transport (rather than the handler)
because `rmcp`'s own service loop enqueues a tool handler's response and
returns almost immediately, then performs the real write+flush on a
separately spawned task with no duration bound — the handler has no way to
await it directly. Wrapping the transport intercepts every flush completion
regardless of which task drives it.

## `forward_or_spawn` — the `None` contract (#644)

Returns `None` only when nothing was ever written to the daemon and local
dispatch is therefore safe: `KHIVE_NO_DAEMON` is set, or the socket is
definitively absent/refused (`NoSocket`). A connect error that does not prove
absence (`Unreachable`) returns `Some(Err)` immediately because the client
cannot know whether a healthy daemon is serving other processes. It never
returns `None` after the real frame has been written — `Some(Ok)`/`Some(Err)`
both mean the caller must not dispatch locally.
Under `KHIVE_DAEMON_STRICT=1`, the `NoSocket` case becomes `Some(Err(..))`
instead (see `fallback_or_reject`) — `KHIVE_NO_DAEMON` itself is unaffected,
since it is the caller's explicit, unconditional opt-out (nothing is ever
recorded or counted for it). Once the real frame IS fully written
(`ParseFailure`/`ProtocolMismatch`), this returns a hard error immediately
instead of killing/respawning/retrying or falling back locally.

Connection classification is intentionally narrow (#1242): `ENOENT` and
`ECONNREFUSED` are `NoSocket`, preserving first-spawn and stale-socket
self-heal. `EACCES`, `EPERM`, and every other indeterminate connect failure
are `Unreachable`; they return the structured `daemon_unreachable` error and
perform zero lifecycle actions in both strict and non-strict mode.
