# State persistence: durable mutation and namespace loading

Source: `crates/khive-pack-brain/src/persist.rs`. Covers how `BrainState` mutations are
made durable (issues #457/#458) and how a namespace's state is cold-loaded/swapped.

## `persist_brain_state_mutation`: why `atomic_unit`, not a manual transaction

This deliberately does not issue a manual `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK` sequence on
a plain `SqlWriter` handle (the trait's former `begin_tx`/`SqlTransaction` surface, retired
entirely, was likewise not an option): under `KHIVE_WRITE_QUEUE=1` that sequence would nest
inside the WriterTask's own per-request `BEGIN IMMEDIATE`, which SQLite rejects ("cannot
start a transaction within a transaction" — the same class of bug `fold_gate.rs`'s
`atomic_unit` conversion fixed, see `crates/khive-pack-brain/docs/api/fold-gate.md`).
Handing the whole append+upsert unit to `atomic_unit` instead means the WriterTask's own
transaction wrapping provides the atomicity on the flag-on path, and `run_manual_atomic_unit`
(khive-db) preserves the old manual-transaction shape byte-for-byte on the flag-off/in-memory
path.

The latest namespace snapshot generation is read through the transaction's `SqlWriter` before
the mutation closure runs. This ordering is the cross-process coherence boundary: SQLite's
`BEGIN IMMEDIATE` serializes competing writers. A process-local snapshot is reused only when
its recorded durable generation exactly matches the row read inside the transaction (which
preserves best-effort hook updates); on a mismatch, the snapshot JSON is fetched, validated,
and used as the mutation base. Avoiding that full decode on the matched steady-state path keeps
the writer exclusion window bounded to work the mutation actually needs. The proposed
`BrainState` is published only after commit.
Both mutation paths serialize the proposed state through a borrowed snapshot view.
Profile and posterior projections retain the existing snapshot semantics, while
bindings, opaque router bytes, and adapter records are serialized without cloning
them into a second owned snapshot. The initial private-state reconstruction and
the encoded JSON allocation remain. Serialization and snapshot-write failures
still roll back the whole atomic unit without publishing the proposed state.
The event and snapshot share a transaction timestamp chosen as
`max(wall_clock, previous_updated_at + 1)`, so a writer that waited behind a newer process
cannot move the replay boundary backward and cause already-snapshotted feedback to replay twice.

`persist_feedback_state_mutation` uses the same generation/rebase boundary, then validates the
selected profile against that authoritative state. For accepted feedback, the implicit-mass fold
and dedup claim (when applicable), public `FeedbackExplicit` event, private `brain_event_log` row,
and replacement snapshot commit in that same atomic unit. An absent or archived profile is
rejected before any of those writes, so a peer lifecycle transition cannot leave a partial
feedback trace after the warm process-local preflight has passed.

Caller-judged `brain.auto_feedback` with a scorer/serve-ledger pair uses the
`Graded` arm in `persist/feedback.rs`: the existing dedup claim and grade backfill
join the public event, private event, and snapshot in that same atomic unit.
It does not enter the implicit-mass gate. A conflicting claim publishes no state;
any append, grade, or snapshot failure rolls back the claim along with the other
writes, leaving an identical retry possible. Manual feedback admission is unchanged.

`brain.auto_feedback(signal="unjudged")` instead uses the existing attributed
`Runtime::events(token).append_event` operation. Its single `feedback_unjudged`
event carries available serve provenance and commits through the event store's
existing queue/direct transaction path. It does not call a profile mutation
helper, append to the private brain log, update a snapshot, claim a scorer pair,
backfill a grade, or create an implicit-mass row. Ledger resolution validates
provenance without writing; even an already-graded pair can carry telemetry.
The kind is excluded before evidence parsing by the existing interpreter/replay
boundary. No default profile is fabricated for profileless telemetry.

The monotonically-raised snapshot `updated_at` is the namespace's durable generation.
`ensure_loaded` checks that primary-key row even for an already-active namespace and reloads
when another process has advanced it. This keeps profile and binding reads coherent across
long-running processes without a scan, polling loop, or new schema column.

## Why `persist_brain_state_mutation` takes `&dyn SqlAccess`, not `&KhiveRuntime`

The only thing this function ever needed from the runtime was its `SqlAccess` handle
(`KhiveRuntime::sql()`). Narrowing the parameter lets tests exercise this function against
a bare `SqlBridge`/`ConnectionPool` (write-queue-enabled via a `PoolConfig` literal,
mirroring `fold_gate.rs`'s routing test) without needing a full file-backed `KhiveRuntime`
and its associated `KHIVE_WRITE_QUEUE` env-var race across this crate's test binary.
