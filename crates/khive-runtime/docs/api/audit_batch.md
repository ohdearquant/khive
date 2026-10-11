# Audit Batching — Lifecycle, Retry, Supervision, and Failure Semantics

`audit_batch.rs` is the ADR-133 seam that takes incidental audit-event writes off the
synchronous request hot path. Instead of every dispatch acquiring the writer to append its own
audit row, callers `submit()` a prepared row into a shared queue; a supervised background driver
drains the queue into generations and commits each generation with one
`EventStore::append_events_idempotent()` call, so concurrent submissions collapse into shared
writer acquisitions instead of paying one acquisition per row.

## Classification: obligation vs. observability

`AuditProducer` names every call site that can submit a row; `classify()` maps each producer to
an `AuditProductionClass` — `DispatchObligation` (a caller-visible outcome the audit trail must
not silently lose) or `PureObservability` (best-effort telemetry). The match has no wildcard arm:
adding a new `AuditProducer` variant without updating `classify()` fails to compile. This is the
enforcement point for D2/D3 — a new audit call site cannot be wired in without an explicit,
reviewed classification decision.

`PureObservability` rows degrade (recorded via `record_degradation`, exposed for tests through
`metrics_snapshot()`) rather than blocking the caller when the batch fails closed.
`DispatchObligation` rows still return `Err` to their waiter on failure — this module does not
change what already-shipped best-effort/strict call sites promise their callers, only how the
underlying write is scheduled.

## Lifecycle

`AuditBatch::new(store, config)` returns an `Arc<AuditBatch>` with no background task running.
The first `submit()` spawns the supervisor (`spawn_supervisor_if_idle`); it exits once its queue
drains and respawns on the next arrival, so an idle batch costs nothing. `Lifecycle` is
`Open → Closing → Closed`, or `Failed(AuditTerminalReason)` from any state once the driver
observes an unrecoverable condition. `Failed` is sticky — `fail_driver` only transitions once;
later callers reaching a failed batch get the recorded terminal reason immediately.

`quiesce()` polls until the queue is empty and no generation is in flight, without closing the
batch to new submissions. `close_and_drain()` moves `Open`/`Closing` to `Closed` (rejecting new
`submit()` calls with `AdmissionClosed` from that point on), waits for outstanding rows to settle,
then joins the retained supervisor `JoinHandle`.

## Generations and retry

Concurrent `submit()` calls that arrive while a driver iteration is draining the queue share the
same generation and the same `append_events_idempotent()` call — this is the batching payoff.
Each generation retries transient storage failures (`WriteQueueFull`, `WriterTaskBusy`, `Pool`,
`Timeout`, `WriterTaskRequestFailed` whose cause is itself transient, and
`WriterTaskTerminated{NotStarted | TransactionRolledBack | SideEffectsUnknown}`) up to
`AuditBatchConfig::max_commit_attempts` with `retry_backoff` between attempts
(`classify_store_error`). `Unsupported("append_events_idempotent")` and any other storage error
are terminal for the generation, not retried.

The writer task wraps any request operation that failed and was rolled back in
`WriterTaskRequestFailed{TransactionRolledBack}`, whatever the cause, so the wrapper is
classified by the error it carries: a `Pool`, `Timeout`, `WriteQueueFull` or `WriterTaskBusy`
cause (a failed COMMIT, or a retryable refusal relayed by the events daemon) or a driver error
carrying SQLite `BUSY`/`LOCKED` is retried like the same error unwrapped. Any other cause, for
example a missing column or a constraint violation, would fail identically on every attempt: the
generation ends as `StoreFailure` after one attempt instead of `RetryExhausted` after all of
them. A `WriterTaskRequestFailed{SideEffectsUnknown}` is retried whatever its cause, since the
append is idempotent.

When a generation ends in failure, `RetryExhausted` or any terminal reason, the last store error
it saw is logged once at `warn` with the attempt count and the reason
(`audit generation failed; its rows were not committed`).

## Admission: refusal vs. deadline expiry (khive#2117, khive#2208)

`submit()` can fail on admission two ways that are not interchangeable, so they carry distinct
`AuditTerminalReason` variants:

- `QueueAdmissionExhausted` — `state.pending.len() >= max_pending_rows` at enqueue time. The row
  is never pushed and never counted in `submitted_rows`: a pure refusal, safe to retry.
- `AdmissionDeadlineExpired` — the row was already pushed and counted when the caller's
  `tokio::time::timeout(admission_deadline, rx)` elapsed waiting for its generation's outcome. By
  that moment the row may still be sitting in `state.pending`, or the driver may have already
  drained it into an in-flight generation — either way it remains enqueued and unresolved, and the
  generation driver commits (or terminally fails) it independently of this caller's timeout, so the
  caller cannot tell from the reason alone whether the row eventually landed, or even which of
  those two states it was in. Retrying is only safe for an idempotent caller.

`pack.rs::append_audit_event_best_effort` treats both as "audit-lane admission pressure": for a
`DispatchObligation` row produced by a verb that is both `VerbCategory::Assertive` AND explicitly
opted in via `VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS` (an explicit, fail-closed allowlist —
`Assertive` alone is not a sound proxy, since some Assertive handlers have their own
durable or accounting-bearing side effects; see that constant's doc comment), either reason
degrades to best-effort instead of failing the dispatch — the read performed no domain write, so
discarding its already-computed result to protect an obligation it does not need as strictly as a
write does inverts the point of serving it (khive#2147, khive#2217). The allowlist's opt-in is
keyed by the owning pack and verb together, not the verb name alone, so a handler registered under
the same name by a different pack never inherits degrade-safety it was not reviewed for. A closed
live-source census classifies every public Assertive handler as either allowlisted or an
incidental-effect exclusion, so newly added Assertive verbs remain fail-closed until reviewed.
Every other obligation failure, and every failure for a non-opted-in verb, is unaffected —
write-side hard-fail semantics are unchanged.

Pack identity for this decision is never taken from the pack's own `PackRuntime::name()` report:
eligibility additionally requires the pack to have been registered through the composition root's
trusted path (`VerbRegistryBuilder::register_boxed`, exercised only by `PackRegistry::register_packs`'s
`inventory`-discovered factories), not the public `VerbRegistryBuilder::register`. A pack loaded
through the untrusted path can claim any `name()` it likes, including an allowlisted one, so
without this third condition a same-named handler from an unreviewed pack could inherit
degrade-safety whenever the real pack of that name was not also loaded. The whole eligibility
decision — pack trust, category, and the `(pack, verb)` allowlist — is precomputed once when
`VerbRegistryBuilder::build` runs, not re-derived per dispatch.

For a successful non-degrade-safe operation, the domain effect may already be committed when its
deferred audit row is enqueued. Those rows, and `GitDigestReceipt` rows, use
`AuditBatch::submit_until_resolved()` (khive#2256): crossing `admission_deadline` emits a warning
but keeps awaiting the same generation receiver, now bounded by a second, larger
`AuditBatchConfig::resolution_deadline` (khive#2331) rather than unbounded — a stalled
`append_events_idempotent()` call must not retain the completed write's caller, its request slot,
and its audit-lane waiter forever, exhausting both request and audit capacity. If
`resolution_deadline` also elapses, the caller gets the dedicated `ResolutionDeadlineExpired`
reason instead of `AdmissionDeadlineExpired`, so a caller (and diagnostics reading the reason) can
tell a merely-slow admission wait apart from a resolution wait that gave up entirely. Either way
the handler is not invoked again and the row is not re-enqueued — the row is left exactly where
the driver holds it for the driver to resolve independently. Pre-enqueue `QueueAdmissionExhausted`
and real store/driver failures still return errors. Ordinary `submit()` retains the bounded
deadline contract for admission-degrade reads, failed/denied outcomes, and pure observability.

## The driver's own append bound and the abandoned-append cap (khive#2331)

`resolution_deadline` bounds only how long a _caller_ keeps waiting; it does nothing to stop the
_driver_ from holding one stalled generation forever. `supervisor_loop` wraps its
`run_generation` child's `.await` in its own `driver_append_deadline` (3x `resolution_deadline`,
derived rather than a separate config field — see the rationale on `driver_append_deadline` in
`audit_batch.rs`). If that elapses, the generation is recorded with
`AuditTerminalReason::DriverAppendAbandoned`, every waiter on it resolves with that reason, and
the loop immediately drains whatever has queued in `pending` since — the row is never re-enqueued
and any already-committed domain effect is never retried. The underlying store call is not
cancelled (it may not be safely abortable mid-write); it is handed to a detached task that drives
it to completion and discards whatever it eventually returns.

Left unbounded, a store whose append never returns would mint one such detached task per
`driver_append_deadline` forever, each retaining up to `max_rows_per_generation` events.
`AuditBatchConfig::max_abandoned_appends` (default 4) caps how many may be outstanding at once.
Before spawning a generation's child, the driver checks the current count: at or above the cap,
the store is treated as wedged and the generation is shed instead — no child task, no store call,
every waiter resolves immediately with `AuditTerminalReason::StoreWedged`, and `flush_failures`
increments. A shed generation costs no store work. As soon as one outstanding append returns
(commit or failure — each recorded on `AuditBatchHealthMetrics::late_append_commits` /
`late_append_failures`, so an operator can see a wedged store later drained), the count drops
below the cap and the next generation attempts a real append again; recovery needs no timer of its
own. Combined, the two bounds cap the retained-buffer growth a wedged store can cause at
`max_abandoned_appends * max_rows_per_generation` rows.

## Supervision and failure ownership (owner ruling R1)

The supervisor loop retains its `JoinHandle` and spawns each generation's commit as its own child
task. A `SupervisorGuard` is armed before the child spawns and disarmed only after the generation
result is classified; if the guard drops still armed — supervisor panic, supervisor task
cancellation, or an early return — its `Drop` impl calls `fail_driver` with `DriverPanicked` or
`DriverCancelled` (distinguished via `std::thread::panicking()`) **before** any background count
restore runs, so no waiter can observe a stale non-failed state after an abnormal supervisor
exit.

`AuditTerminalReason` also has two variants for the child generation task itself:
`DriverJoinLost` when the child's `JoinHandle` can no longer be awaited (its result is
unrecoverable), and `DriverExitedInconsistent` when the child returns successfully but the state
it reports is not one of the recognized terminal outcomes. Both fail the batch the same way any
other terminal reason does — every pending and in-flight waiter receives `Err(reason)`.

## Test-only surface

`AuditBatchSnapshot`, `AuditBatchMetricsSnapshot`, `audit_delta()`, and the `fault_injection`
module (`arm_child_panic`, `arm_child_cancel`, `arm_supervisor_panic`,
`arm_supervisor_sleep_before_spawn`, `arm_join_lost`, `arm_inconsistent_exit`) are gated behind
`#[cfg(any(test, feature = "test-internals"))]` / `feature = "fault-injection"` respectively.
`audit_delta()` does checked, monotonic subtraction between two snapshots and rejects a regressed
counter or a shrunk generation history — a snapshot pair from an actual run should never produce
one. These items are `pub`, not `pub(crate)`, because `tests/adr133_audit_batch.rs` compiles as a
separate crate and cannot reach `pub(crate)` items; this is a deliberate visibility widening with
no new wire vocabulary (no MCP verb, no wire field) attached to it.

## Recall telemetry: `AuditProducer::RecallExecuted`

The recall handler submits `RecallExecuted` through the same registry-owned batch
as dispatch audit, using its existing `PureObservability` classification. The
bounded background serve job already holds a clone of the registry; it needs no
runtime-held batch or second batch instance. Its semantic event is built once and
stamped from the sealed caller token before reaching the registry's raw sink, so a
per-request namespace or actor override survives a differently configured registry.

Recall results retain their existing response boundary: telemetry runs in the
tracked background job, after any brain serve-ledger dispatch. Visibility requires
that producer to submit and the batch to settle; this does not promise that the row
is visible when the recall response returns. Producer admission and execution are
still bounded. Shutdown owners should drain producers before closing the batch when
they require their rows to be admitted, then drain accepted rows before writer teardown.
A late producer against a closing batch reports a best-effort refusal.

A batch refusal or terminal failure warns with its typed reason without changing
the recall handler result. Generation failures use the existing pure-observability
degradation counters; immediate preflight/closed/full-queue refusals do not promise
a `degraded_rows` increment. A timed-out or cancelled waiter does not remove an
accepted row, and the emitter never resubmits or direct-appends after a batch error:
the original generation may still commit. Recall's separate dispatch obligation and
brain accounting keep their own durability rules.

When the registry has no batch, recall retains its token-decorated runtime accessor
and direct best-effort append, including its acquisition/append warnings. This is a
legacy no-batch route, not a fallback for a failing configured batch. ADR-133's
baseline and Amendment 3 remain Proposed; this producer wiring changes neither status.
