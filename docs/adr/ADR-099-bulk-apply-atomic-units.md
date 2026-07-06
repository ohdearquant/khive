# ADR-099: Cross-Op Atomicity for Bulk Apply — Atomic Units over the Single-Writer Seam

**Status**: Accepted (2026-07-06)
**Date**: 2026-07-06
**Issue**: #195 (true cross-op atomicity for `exec --ops-file` bulk apply)
**Depends on**: ADR-067 (Write-Owner Daemon — single-writer task and the `atomic_unit` seam), ADR-005 (storage capability traits), ADR-016 (request DSL), ADR-091 (bounded read-transaction lifetime; Plank 0 open-transaction registry)
**Amends**: none (a companion ADR-067 amendment records the now-final write-path inventory; see D6)

---

## Context

### What bulk apply does today

The `exec --ops-file` command reads a newline-delimited file of `{"tool": ..., "args": ...}`
operations, groups them into chunks of 100, and dispatches each chunk as a **parallel batch**
through the same in-process dispatch path the `request` tool uses (`apply_ops_file` →
`dispatch_request_local` in `crates/kkernel/src/exec.rs`). The daemon fast-path is deliberately
bypassed for bulk apply, so the ops run against an in-process runtime.

Each op inside a chunk dispatches through its own verb handler, and each handler acquires a
write path and commits independently. Two consequences follow:

1. **No rollback across ops.** A failure in op 47 of a 100-op chunk leaves ops 1..46 committed.
   There is no rollback across ops within a chunk, and none across chunks.
2. **Chunk boundaries are a transport artifact.** The chunk size of 100 exists to bound frame
   size, not to define a meaningful unit of work. All-or-nothing "per chunk" would make the
   atomic boundary depend on an arbitrary transport constant.

This is acceptable for idempotent mass updates (the original bulk-apply value: one process
instead of N spawns, parse-first safety, dry-run preview). It is **not** acceptable for
cleansing operations that require all-or-nothing semantics, which is the stated end goal of
issue #195.

### What the single-writer work already gives us

ADR-067 introduced a single write-owner task and, as part of that work, shipped a
transaction-enlisted execution seam on the `SqlAccess` trait
(`crates/khive-storage/src/sql.rs`):

```rust
async fn atomic_unit(&self, op: AtomicUnitOp) -> StorageResult<Box<dyn Any + Send>>;
```

`atomic_unit` runs a caller-supplied closure **inside one open write transaction**. Where the
single-writer task is active, the closure runs on that task's transaction for the request, so it
cannot compete with the writer for the SQLite write lock. Where no writer task applies (flag off,
no async runtime, or an in-memory pool), the closure runs under a manual
`BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK` on a writer handle — byte-identical to the pre-ADR-067
behavior. The closure must issue DML only; the transaction boundary is owned entirely by
`atomic_unit`. This seam is already in production use by the fold-gate write path.

This ADR builds the cross-op atomicity of issue #195 **on that existing seam**, rather than
introducing a new transaction primitive or threading a transaction handle through every verb
handler signature.

### The remaining standalone-writer hole

The `SqlAccess` trait also exposes `begin_tx(options)`, which returns a live
`Box<dyn SqlTransaction>` (a handle supporting incremental `execute`, then `commit`/`rollback`).
Unlike `atomic_unit`, `begin_tx` opens a **standalone connection** and issues its own
`BEGIN IMMEDIATE` outside the single-writer task. It is the one remaining write path that can
compete with the writer for the WAL write lock.

An exhaustive audit of the current tree finds exactly **one live production caller** of
`begin_tx`: the session-mirror ingest path (`write_events_and_cursor` in
`crates/khive-pack-session/src/mirror/ingest.rs`), which opens a transaction, loops over parsed
session events inserting session/message rows and advancing a file cursor, and commits once. Its
no-partial-state contract (the cursor advances only if the row writes commit) is covered by a
regression test in the same file. All other references to `begin_tx` are tests, or comments in
the brain pack explaining why that code uses `atomic_unit` instead.

Closing this hole is in scope here because the same mechanism that gives bulk apply its atomic
unit also gives session-mirror ingest a home that does not open a competing connection.

---

## Decision

Add cross-op atomicity as an **opt-in restricted atomic unit** built on the existing
`atomic_unit` seam, and retire the last standalone-writer caller onto the same seam.

Concretely:

- **D1**: Bulk apply gains an `--atomic` flag. Under `--atomic`, the whole file is executed as
  one atomic unit: all ops commit, or none do. The default (no flag) is unchanged per-op
  behavior.
- **D2**: The atomic unit executes through the **real verb handlers**, enlisted in one shared
  `atomic_unit` transaction via an ambient write context — not by threading a transaction handle
  through every handler signature, and not by compiling ops to raw SQL that bypasses handler
  logic.
- **D3**: Under `--atomic`, only **write-shaped verbs** are admissible; read/query/recall verbs
  are rejected at parse time. This bounds the work held inside the open transaction to fast DML
  and preserves the single-writer latency guarantees ADR-067 established.
- **D4**: The failure envelope is **additive**. Non-atomic batches are byte-identical to today.
  An atomic unit that aborts reports the failing op index and marks every op rolled back.
- **D5**: `begin_tx`'s single live caller (session-mirror ingest) is converted to `atomic_unit`,
  closing the standalone-writer hole. The `begin_tx` trait method is then removed if no non-test
  caller remains (verified at implementation time).

The MCP `request` wire tool is **not** changed. `--atomic` is a `kkernel` CLI concern; the wire
contract in ADR-016/ADR-023 and the agent-facing tool description are untouched.

---

## Per-fork decisions

### D1 — Mechanism for cross-op atomicity

**Decision: reuse `atomic_unit` as the single transaction boundary; enlist real handlers in it
via an ambient write context.**

A new in-process entry point (working name `dispatch_request_atomic`) opens one `atomic_unit`
and dispatches the file's ops **inside** the resulting transaction. Each op runs through its
normal verb handler. The handler's store-level `with_writer` (and, critically, its reads — see
"read-your-writes" below) resolve to the ambient transaction's connection for the duration of the
unit, instead of opening their own writer or reading from the concurrent reader pool. When no
atomic unit is active (every existing code path), `with_writer` behaves exactly as it does today.

The ambient context is scoped strictly to the atomic unit (a task-local or an extension of the
existing open-transaction registry from ADR-091), set on entry and cleared on exit. It is never
observable outside the unit.

**Read-your-writes requirement.** Inside the unit, op _N_ must observe op _N-1_'s uncommitted
writes (e.g. a `link` that references an entity created by an earlier op in the same file). Under
WAL, the concurrent reader-pool connections do not see the writer's open transaction. Therefore,
while an atomic unit is active, a handler's reads MUST route to the ambient transaction
connection, not the reader pool. This is a correctness requirement, not an optimization, and has
a dedicated acceptance test.

**Alternatives considered:**

- **(a) Thread a `SqlTransaction` handle through `dispatch_request_local` → the registry →
  every handler signature (the ADR-067 sketch).** Rejected. The blast radius is every handler
  signature in every pack, plus the runtime service methods those handlers call. The ambient-
  context approach achieves the same enlistment by changing the store-level `with_writer`/reader
  seam — the same, already-migrated surface ADR-067 touched — without rewriting handler
  signatures.
- **(b) Compile write-shaped ops to raw `SqlStatement`s and run them directly in one
  `atomic_unit`, bypassing handlers.** Rejected. Verb handlers own non-trivial logic (identifier
  generation, dedup, endpoint validation, FTS/vector index maintenance, secret masking).
  Re-deriving that as raw SQL in the bulk-apply crate duplicates it and guarantees drift. The
  standing lesson from the fold-gate work is explicit: reuse the insert logic behind the seam;
  do not duplicate it into the consuming crate.
- **(c) Writer lease — the writer task grants an exclusive long-lived transaction slot held
  across the whole dispatch.** Rejected as the general mechanism. Holding the single writer's
  `BEGIN IMMEDIATE` open across N arbitrary verb dispatches — each potentially doing slow reads,
  ANN recall, or service calls — reintroduces exactly the long-held-write-lock starvation that
  ADR-067 eliminated. A lease is only safe if what runs under it is bounded to fast DML, which is
  what D3's write-verb restriction already enforces; at that point the lease is just `atomic_unit`
  with extra machinery. The `atomic_unit` seam is the lease, minus the wedge risk.
- **(d) Savepoint-per-op composition.** Adopted as a **component of the chosen design**, not an
  alternative to it. Inside the one `atomic_unit`, each op is wrapped in a named SAVEPOINT so a
  failure rolls the unit back to a known point and reports the failing op index. This is the same
  SAVEPOINT-per-request hierarchy ADR-067 already uses for per-op isolation, reused here for
  attribution.

### D2 — Unit of atomicity for `--ops-file`

**Decision: whole-file, opt-in via `--atomic`, default off (per-op, unchanged). Bounded by a
configurable op-count guard.**

- **Whole-file, not per-chunk.** The 100-op chunk is a transport-framing constant; all-or-nothing
  across a chunk boundary is not a semantically meaningful unit. A cleansing file is atomic as a
  whole or not at all.
- **Opt-in.** `--atomic` is off by default. Idempotent mass updates keep today's non-atomic,
  per-op-independent behavior with no latency change.
- **Bounded.** A whole-file atomic unit holds the single writer for the full duration of the
  unit. The load harness that qualified ADR-067 already shows multi-second write-slot tails under
  concurrency, so an unbounded 10k-op atomic file is a product-visible stall for every other
  writer on the process. Under `--atomic`, a file exceeding a configurable maximum op count
  (recommended default on the order of a few thousand ops; final value tuned against the harness)
  is **rejected before execution** with a clear error directing the caller to split the file or
  run without `--atomic`. This keeps the maximum writer-hold predictable and bounded.

**Alternatives considered:**

- **Per-chunk atomicity (all-or-nothing per 100).** Rejected: the boundary is arbitrary and would
  silently change if the chunk constant changed. It also gives the caller no guarantee they can
  reason about ("ops 1..100 committed but 101..137 did not" is not all-or-nothing).
- **Always atomic (no flag).** Rejected: it would impose the whole-file writer-hold and the
  write-verb restriction on the common idempotent-update case, and would break callers who rely
  on partial progress of a large idempotent file.

### D3 — Admissible verbs under `--atomic`

**Decision: restrict `--atomic` to write-shaped verbs; reject read/query verbs at parse time.**

Only mutation verbs may appear in an `--atomic` file (create, update, delete, link, merge,
remember, and the task-lifecycle and message mutations, plus the propose/review/withdraw
governance verbs). Read/search/recall/query/traverse/list/get verbs are rejected before the unit
opens, with an error naming the offending line and verb.

Rationale: the atomic unit holds the single writer's transaction open for its whole duration.
Restricting the unit to fast DML keeps that hold short and predictable and prevents a slow read
(ANN recall, large search) from extending the write-lock hold — the precise failure ADR-067
removed. Handler-internal reads (endpoint validation, dedup lookups) are fine: they are fast,
run on the ambient transaction connection, and are necessary for read-your-writes.

**Alternative considered: allow arbitrary verbs inside the unit.** Rejected: it reintroduces the
long-held-write-lock wedge and conflates "atomic mutation" with "read-modify-write transaction,"
a materially larger and riskier feature that issue #195 does not ask for.

### Daemon coexistence under `--atomic`

Bulk apply runs in-process and does not traverse the daemon. When a live daemon is serving
the same database file, an `--atomic` run therefore holds a **cross-process**
`BEGIN IMMEDIATE` on that file for the duration of the unit, while the daemon's write owner
continues to operate inside its own process. The single-writer guarantee is per-process; the
two processes coordinate through SQLite's file-level write lock.

**Decision: accept bounded cross-process coexistence; do not route `--atomic` through the
daemon.**

- This coexistence is not new. The non-atomic bulk apply already writes cross-process
  against a live daemon today — each op takes its own short-lived write lock. `--atomic`
  changes the **duration** of the hold, not its existence, and the duration is exactly what
  the D2 op-count guard bounds.
- Expected daemon-side behavior during the atomic window: daemon writes wait on the
  configured `busy_timeout` and proceed when the unit commits. A window shorter than the
  busy timeout delays daemon writes without failing them; a window that exceeds it surfaces
  `SQLITE_BUSY` to daemon callers. The op-count guard default is therefore sized (from
  harness measurement) so the worst-case hold stays well inside the default busy timeout,
  and the `--atomic` documentation directs operators to run large atomic cleanses in a
  maintenance window or against an idle daemon.
- **Alternative considered: route `--atomic` through the daemon when one is live, so the
  daemon's write owner holds the unit.** Rejected for this ADR. It requires a new daemon
  bulk-transaction protocol surface, reintroduces the transport frame cap that in-process
  bulk apply exists to avoid, and creates a behavioral fork (flag semantics differ by
  daemon liveness). If operational experience shows the bounded window is insufficient,
  daemon-routed atomicity can be a follow-up that reuses this ADR's ambient-context
  machinery unchanged.
- **Alternative considered: refuse `--atomic` when a live daemon is detected.** Rejected:
  liveness detection is racy (a daemon may start mid-unit), and a hard refusal blocks the
  legitimate bounded case the guard already covers.

### D4 — Error response shape for partial-failure rollback

**Decision: additive envelope. Non-atomic batches unchanged; atomic-unit abort reported
explicitly.**

- Non-atomic (default) responses are **byte-identical** to today: each op yields its own
  `{ok, tool, result | error}` and a failure never aborts siblings.
- Under `--atomic`, on the first failing op the whole unit rolls back. The response reports:
  - the failing op's index and its error,
  - that the unit was rolled back (no op committed),
  - the remaining ops marked not-committed rather than succeeded.

  A representative shape (final field names settled in implementation):

  ```json
  {
    "atomic": true,
    "committed": false,
    "failed_op_index": 47,
    "error": "<message from the failing op>",
    "ops": [ { "index": 0, "committed": false, "rolled_back": true }, ... ]
  }
  ```

- On full success, `{ "atomic": true, "committed": true, ... }` with the per-op results.

The `atomic`, `committed`, `failed_op_index`, and `rolled_back` fields are **additive**; they
appear only on the atomic path. No field is removed or repurposed on the non-atomic path, so no
existing consumer breaks.

**Alternative considered: reuse the existing per-op `ok/error` array with no new fields.**
Rejected: a caller could not distinguish "this op failed but siblings committed" (non-atomic)
from "this op failed so the whole unit rolled back" (atomic). The distinction is the entire point
of the feature and must be explicit in the envelope.

### D5 — Fate of `begin_tx()`

**Decision: convert the single live caller to `atomic_unit`, closing the standalone-writer hole;
then remove the `begin_tx` trait method if no non-test caller remains.**

The session-mirror ingest path (`write_events_and_cursor`) is a clean `atomic_unit` shape: it
opens a transaction, performs a sequence of DML statements (session upsert, message insert,
cursor advance), and commits once, with no branch-on-read logic that needs a live incremental
handle. It converts directly: the loop body becomes the `atomic_unit` closure; the cursor advance
is the last statement in the closure; the all-or-nothing contract is preserved because
`atomic_unit` commits once at the end or rolls the whole closure back.

Once converted, `begin_tx` has no live production caller. The standalone-writer hole is closed by
**removing the caller**, independent of whether the method itself is deleted. Implementation then:

1. Greps for any remaining non-test caller of `begin_tx` and of the `SqlTransaction` trait.
2. If none remains and the `SqlTransaction` handle is not load-bearing forward-deployed
   infrastructure elsewhere, removes `begin_tx` from `SqlAccess` and its `SqlBridge`
   implementation, and collapses the now-dead `SqlTransaction` machinery.
3. If `SqlTransaction`/the open-transaction registry is genuinely forward-deployed
   infrastructure, keeps the trait but leaves `begin_tx` with **no production caller** and a doc
   note that production atomicity goes through `atomic_unit`. Either way, the competing-writer
   hole is closed because the live caller is gone.

Removing a method from `SqlAccess` touches the storage trait surface (the trait declaration, the
`SqlBridge` implementation, and test callers). This trait is internal to the runtime; there is no
external SDK consuming it, so the change is contained.

**Alternatives considered:**

- **(i) Migrate `begin_tx` onto the writer task as a lease.** Rejected for the same reason the
  lease was rejected in D1: a live incremental transaction handle held across caller-driven calls
  is exactly the long-held-lock shape ADR-067 removed, and the one live caller does not need an
  incremental handle — it needs one atomic closure, which `atomic_unit` already provides.
- **(iii) Keep `begin_tx` standalone as a permanent documented exemption.** Rejected. As long as a
  live caller opens a standalone `BEGIN IMMEDIATE` outside the writer, the single-writer guarantee
  has a hole under concurrency. The exemption only made sense while there was no seam to move the
  caller to; `atomic_unit` is that seam.

### D6 — Document structure (F5)

**Decision: confirm the split.** This follow-up ADR carries the design (D1–D5). A **separate,
small ADR-067 amendment** records the now-final write-path facts as merged: `begin_tx` as the sole
converted-away live caller, the top-level maintenance seam
(`execute_script_top_level`) as shipped, and any unmanaged write site surfaced during the
inventory (e.g. an orphan-sweep path in the vector store) dispositioned MIGRATE or EXEMPT.

Rationale: the amendment is a documentation-truth correction to an already-accepted ADR and can
land immediately and independently; the design ADR is a forward decision that goes through the
normal design-review gate. Keeping them separate matches the repository convention of not bundling
a documentation correction with a design change, and lets neither block the other.

**Alternative considered: one combined document.** Rejected: it couples an immediately-landable
inventory correction to a design that needs review, and mixes "record what merged" with "decide
what to build."

---

## Migration steps

1. Add the ambient write-context scope (task-local or an extension of the open-transaction
   registry) and teach the store-level `with_writer` and reader seams to resolve to the ambient
   transaction connection when a unit is active. No behavior change when no unit is active.
2. Add the in-process `dispatch_request_atomic` entry that opens one `atomic_unit`, sets the
   ambient context, dispatches the file's ops inside it with a SAVEPOINT per op, and clears the
   context on exit.
3. Add `--atomic` to `exec --ops-file`. Under the flag: reject read-shaped verbs at parse time
   (D3); reject files over the op-count guard (D2); route the whole file through
   `dispatch_request_atomic`.
4. Implement the additive atomic failure envelope (D4).
5. Convert `write_events_and_cursor` (session-mirror ingest) from `begin_tx` to `atomic_unit`
   (D5), preserving the cursor-advances-only-on-commit contract.
6. Grep for remaining non-test `begin_tx`/`SqlTransaction` callers; remove `begin_tx` from
   `SqlAccess` and its implementation if none remains, else document the no-live-caller state.
7. Land the companion ADR-067 amendment separately (D6).

Schema is unchanged; no migration file is required. Pack rules are unchanged; this is additive.

---

## Acceptance criteria

- **Atomic rollback end-to-end.** An `--atomic` ops-file with a deliberate mid-file failure
  (e.g. a valid op followed by an op that must fail, followed by more valid ops) leaves the
  database **unchanged**: none of the file's writes are present after the run, and the response
  reports the failing op index with `committed: false`.
- **Read-your-writes within a unit.** An `--atomic` file where a later op references an entity
  created by an earlier op in the same file succeeds, proving later ops observe earlier
  uncommitted writes through the ambient transaction connection.
- **Write-verb restriction.** An `--atomic` file containing a read/recall/query verb is rejected
  before any write occurs, naming the offending line and verb.
- **Op-count guard.** An `--atomic` file exceeding the configured maximum op count is rejected
  before execution with an actionable error.
- **Non-atomic parity.** With `--atomic` absent, bulk apply output and per-op semantics are
  byte-identical to the pre-change behavior (per-op independence preserved; a failing op does not
  abort siblings).
- **Single-writer concurrency test (mandatory).** Concurrent session-mirror ingest plus normal
  write traffic, with the single-writer task active, shows **no competing writer**: after
  converting ingest to `atomic_unit`, no standalone `BEGIN IMMEDIATE` is issued outside the writer
  owner on any concurrent path. This path is not exercised by the general write-load harness and
  must be covered explicitly.
- **Daemon coexistence under `--atomic`.** With a live daemon serving the same database file
  and carrying concurrent write traffic, an `--atomic` run within the op-count guard
  commits successfully, the daemon's concurrent writes complete (delayed at most by the
  atomic window, none lost, no corruption), and any daemon-side `SQLITE_BUSY` is observed
  only if the window is deliberately driven past the busy timeout. This cross-process case
  is covered by neither the general write-load harness nor the in-process test suite and
  must be exercised explicitly.
- **Revert-companion test.** The key session-ingest and atomic-rollback tests demonstrably
  **fail** against the pre-change shape (standalone `begin_tx`, per-op non-atomic bulk apply),
  proving the tests are non-vacuous.
- **Session-ingest no-partial-state preserved.** The existing regression that asserts the mirror
  cursor advances only when row writes commit passes after the `atomic_unit` conversion.

---

## Open questions

1. **Ambient-context carrier.** Task-local versus extending the existing open-transaction
   registry. Both scope the context to the unit; the registry already tracks open transactions
   and may be the more coherent home. A short implementation spike should pick one and confirm it
   composes with the reader-routing requirement.
2. **Op-count guard default.** The exact maximum-ops threshold for `--atomic` should be set from a
   harness measurement of writer-hold time versus op count, not guessed. Ships configurable with a
   conservative default.
3. **`SqlTransaction` retention.** Whether the `SqlTransaction` trait and the open-transaction
   registry are forward-deployed infrastructure worth keeping after `begin_tx`'s last caller is
   gone, or dead machinery to collapse. Resolved by the implementation-time grep in migration
   step 6.

---

## References

- ADR-067 — Write-Owner Daemon: single-writer task, the `atomic_unit` seam, per-request SAVEPOINT
  isolation, and the `--ops-file` cross-op atomicity deferral this ADR resolves.
- ADR-005 — Storage capability traits: `SqlAccess`, `atomic_unit`, `begin_tx`, `SqlTransaction`.
- ADR-016 — Request DSL and the per-op independence contract this ADR preserves on the non-atomic
  path.
- ADR-091 — Bounded read-transaction lifetime and WAL checkpoint escalation; its Plank 0 open-transaction registry is the candidate ambient-context carrier.
- Issue #195 — true cross-op atomicity for `exec --ops-file` bulk apply.
