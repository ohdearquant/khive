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
Each generation retries transient storage failures (`WriteQueueFull`, `WriterTaskBusy`,
`WriterTaskTerminated{NotStarted | TransactionRolledBack | SideEffectsUnknown}`) up to
`AuditBatchConfig::max_commit_attempts` with `retry_backoff` between attempts
(`classify_store_error`). `Unsupported("append_events_idempotent")` and any other storage error
are terminal for the generation, not retried.

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
accounting-bearing side effects; see that constant's doc comment), either reason degrades to
best-effort instead of failing the dispatch — the read performed no domain write, so discarding
its already-computed result to protect an obligation it does not need as strictly as a write does
inverts the point of serving it (khive#2147, khive#2217). Every other obligation failure, and
every failure for a non-opted-in verb, is unaffected — write-side hard-fail semantics are
unchanged.

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

## Known scope gap: `AuditProducer::RecallExecuted`

`RecallExecuted` is classified (`PureObservability`) but not wired to a live call site.
`khive-pack-memory`'s recall handler (`emit_recall_executed_event`) only holds a `&KhiveRuntime`
handle, not a `&VerbRegistry` — `AuditBatch` is owned by `VerbRegistry` (`pack.rs`), and
`KhiveRuntime`/`runtime.rs` is a separate ownership boundary. Wiring this producer requires either
threading a batch handle into `KhiveRuntime` or moving the recall audit emission to a layer that
already holds the registry; both are structural changes out of scope here. The variant carries an
`#[allow(dead_code)]` with this explanation rather than being wired against a call site it cannot
reach from this module.
