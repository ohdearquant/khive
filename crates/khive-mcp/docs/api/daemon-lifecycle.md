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

`kill_stale_daemon_inner`'s wait for the incumbent's exit can run for its
full `exit_timeout`, which is long enough for a second recoverer — one that
started its own kill+spawn before this one committed to the recoverer lock —
to have already bound a replacement. Rather than spawn on top of that, the
kill is followed by one more `probe_daemon_identity` call before `spawn()`
runs: `Alive` there means a peer's replacement already answered, so this
recoverer returns `Skipped` instead of double-spawning; `Timeout`/
`LockContended` fall back to `Uncertain` for the same NEVER-KILL-SLOW reason
as the initial probe.

The exit wait itself (`wait_for_process_exit`/`process_is_alive`) first
attempts a non-blocking `waitpid(pid, WNOHANG)` on every poll
(`reap_exited_child`): if this process is the incumbent's parent and it has
already exited, that positively reaps it and confirms the exit immediately.
This matters because `forward_or_spawn` drops the `Child` handle for a killed
incumbent without calling `wait()` on it — without the reap, the kernel keeps
its exit status pending as a zombie table entry for the life of this process.
A PID this process does not own fails that probe with `ECHILD` and falls
through unchanged to the existing `kill(pid, 0)` + `ps -o stat=` liveness
check.

Two test-only barriers (`RECOVERY_RACE_BARRIER`, `SPAWN_COMMIT_BARRIER`)
force concurrent recoverers under test to reach, respectively, the
classification-complete point and the commit-to-spawn point at the same
instant — without them, normal tokio scheduling lets one recoverer finish
before the other's rounds even observe anything, and the two-recoverer
regression test would pass even with the recoverer lock deleted.
`SPAWN_COMMIT_BARRIER` falls through after its bound rather than waiting
forever, since a recoverer still blocked on the *real* recoverer lock must
not be forced to rendezvous.

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
blocking `flock`, so `DEAD_CONFIRM_ROUNDS` bounded probe *count* but not
elapsed *time* — a wedged lock holder blocked recovery forever. A
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
`fire_pending_self_heal` fires on the next successful flush of *any*
message, not specifically the mismatch response's own flush — on this
bridge's dominant single-request-at-a-time usage those are the same event,
but a genuinely concurrent second in-flight request could in principle flush
first. Strictly better than the pre-fix timer (which could fire before *any*
flush completed), and the same class of pre-existing risk, not a new one.

`SelfHealOnFlushTransport` wraps the transport (rather than the handler)
because `rmcp`'s own service loop enqueues a tool handler's response and
returns almost immediately, then performs the real write+flush on a
separately spawned task with no duration bound — the handler has no way to
await it directly. Wrapping the transport intercepts every flush completion
regardless of which task drives it.

## `forward_or_spawn` — the `None` contract (#644)

Returns `None` only when nothing was ever written to the daemon and local
dispatch is therefore safe: `KHIVE_NO_DAEMON` is set, or no daemon socket
could be reached (`NoSocket`). It never returns `None` after the real frame
has been written — `Some(Ok)`/`Some(Err)` both mean the request's fate is
already decided at the daemon and the caller must not dispatch locally.
Under `KHIVE_DAEMON_STRICT=1`, the `NoSocket` case becomes `Some(Err(..))`
instead (see `fallback_or_reject`) — `KHIVE_NO_DAEMON` itself is unaffected,
since it is the caller's explicit, unconditional opt-out (nothing is ever
recorded or counted for it). Once the real frame IS fully written
(`ParseFailure`/`ProtocolMismatch`), this returns a hard error immediately
instead of killing/respawning/retrying or falling back locally.
