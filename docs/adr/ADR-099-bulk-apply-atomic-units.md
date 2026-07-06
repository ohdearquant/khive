# ADR-099: Cross-Op Atomicity for Bulk Apply — Prepared Write Plans over the Single-Writer Seam

**Status**: Accepted (2026-07-06)
**Date**: 2026-07-06
**Issue**: #195 (true cross-op atomicity for `exec --ops-file` bulk apply)
**Depends on**: ADR-067 (Write-Owner Daemon — single-writer task and the `atomic_unit` seam), ADR-005 (storage capability traits), ADR-016 (request DSL)
**Amends**: none (a companion ADR-067 amendment records the now-final write-path inventory; see D6)

---

## Context

### What bulk apply does today

The `exec --ops-file` command reads a newline-delimited file of `{"tool": ..., "args": ...}`
operations, groups them into chunks of 100, and dispatches each chunk as a **parallel batch** of
independent ops through the same in-process dispatch path the `request` tool uses (`apply_ops_file`
→ `dispatch_request_local` in `crates/kkernel/src/exec.rs`). The daemon fast-path is deliberately
bypassed for bulk apply, so the ops run against an in-process runtime. The ops-file JSON form is
independent ops: `$prev` substitution is not available, so one op cannot reference an identifier
produced by an earlier op in the same file unless that identifier is caller-known.

Each op inside a chunk dispatches through its own verb handler, and each handler acquires a write
path and commits independently. Two consequences follow:

1. **No rollback across ops.** A failure in op 47 of a 100-op chunk leaves ops 1..46 committed.
   There is no rollback across ops within a chunk, and none across chunks.
2. **Chunk boundaries are a transport artifact.** The chunk size of 100 exists to bound frame
   size, not to define a meaningful unit of work.

This is acceptable for idempotent mass updates (the original bulk-apply value: one process instead
of N spawns, parse-first safety, dry-run preview). It is **not** acceptable for cleansing
operations that require all-or-nothing semantics — the salience-recalibration and mass-cleanse use
cases that motivate issue #195. Those are mass mutations over existing rows (`update` / `delete` /
`link`), not create-then-reference chains with intra-file dependencies.

### The transaction seam that shipped with ADR-067, and its hard constraint

ADR-067 shipped a transaction-enlisted execution seam on the `SqlAccess` trait
(`crates/khive-storage/src/sql.rs`):

```rust
async fn atomic_unit(&self, op: AtomicUnitOp) -> StorageResult<Box<dyn Any + Send>>;
```

`atomic_unit` runs a caller-supplied closure inside one open write transaction. It has a **hard
constraint that governs this entire ADR**: on the single-writer path (the production daemon), the
closure is executed on the writer task's connection through a single-poll driver
(`block_on_sync`, `crates/khive-db/src/sql_bridge.rs`). That driver returns a typed error if the
closure's future ever reaches a real suspension point. The closure may therefore issue only
**synchronous DML** against the provided writer (the `InlineWriter`, whose methods are pure
rusqlite calls that never await). A closure that performs asynchronous work — embedding, ANN
warming, a network or channel round-trip — suspends, hits the error path, and fails.

This constraint is not incidental; it exists because the writer task drives the closure inside a
synchronous `spawn_blocking` context and must not block that context on external async work while
holding the single write connection. It is the correct constraint, and this ADR is designed to
honor it rather than to relax it.

> **Design invariant — the Synchronous-DML Atomic-Unit contract (call it the _atomic-unit
> suspend-free invariant_):** any closure passed to `SqlAccess::atomic_unit` must complete on its
> first poll — it may issue only synchronous DML against the provided writer and must never reach a
> real suspension point (no embedding, no ANN warming, no service or channel `await`). On the
> single-writer path a violation fails loudly through `block_on_sync`; on the flag-off path it
> would silently succeed, so the invariant is a correctness contract the caller must uphold, not
> something the type system enforces. Every decision in this ADR that touches the seam (D1, D3, D5)
> is justified against this invariant, and migration step 0 writes it into the seam's own
> doc-comments so the next consumer is warned in the code, not only here.

**Consequence for the design:** the cross-op atomic unit cannot dispatch full verb handlers
inside `atomic_unit`, because real write handlers suspend. `create_entity` awaits
`embed_document_with_model` before its vector insert (`crates/khive-runtime/src/operations.rs`);
`memory.remember` embeds and then invalidates and warms ANN state
(`crates/khive-pack-memory/src/handlers/remember.rs`). Any design that wraps those handlers in one
`atomic_unit` would either hit the suspension error on the daemon path or behave differently
depending on whether the single-writer flag is on — an unacceptable liveness-dependent fork.

### The remaining standalone-writer hole in this ADR's scope

The `SqlAccess` trait also exposes `begin_tx(options)`, which opens a **standalone connection** and
issues its own `BEGIN IMMEDIATE` outside the single-writer task. Within this ADR's scope, its one
live production caller is the session-mirror ingest path (`write_events_and_cursor` in
`crates/khive-pack-session/src/mirror/ingest.rs`): it opens a transaction, loops over parsed
session events inserting session/message rows and advancing a file cursor, and commits once. Its
no-partial-state contract (the cursor advances only if the row writes commit) is covered by a
regression test in the same file. All other `begin_tx` references are tests or explanatory
comments.

A second unmanaged write path — the vector store's `orphan_sweep`, which opens its own
`BEGIN IMMEDIATE` outside the writer task — is **out of scope here**. Its conversion is tracked
and in flight as a separate change under the companion ADR-067 amendment (D6). This ADR's
competing-writer statements and acceptance criteria are therefore scoped specifically to the
`begin_tx` / session-ingest path and make no claim about `orphan_sweep`.

---

## Decision

Add cross-op atomicity as an **opt-in, prepared-write-plan** unit, and retire the session-ingest
`begin_tx` caller onto the same `atomic_unit` seam.

- **D1 (mechanism)**: `--atomic` runs in two phases. First, an **async prepare pass** materializes
  each admissible op into a synchronous write plan (all suspension-bearing work — validation,
  identifier generation, and, for verbs that need it, embedding — happens here, outside any
  transaction). Second, a **single `atomic_unit`** applies every plan as synchronous DML under a
  per-op SAVEPOINT, committing all or rolling back all. `atomic_unit` therefore only ever runs
  synchronous DML, honoring its `block_on_sync` contract identically on the flag-on and flag-off
  paths.
- **D2 (unit)**: whole-file, opt-in via `--atomic`, default off (per-op, unchanged), bounded by a
  configurable op-count guard.
- **D3 (admissible verbs)**: only verbs that expose a prepare/apply seam are admissible; the v1
  set is the DML-only mutations. Embedding-bearing verbs and all read verbs are rejected at parse
  time. Including embedding-bearing verbs is a named extension path, not v1.
- **D4 (error shape)**: the CLI response preserves the existing `results` + `summary` shape and
  adds an `atomic` block; nothing is removed or repurposed.
- **D5 (`begin_tx`)**: convert the session-ingest caller to `atomic_unit` (its closure is verified
  suspension-free), closing that standalone-writer hole; remove the trait method if no non-test
  caller remains.

The MCP `request` wire tool is **not** changed. `--atomic` is a `kkernel` CLI concern; the wire
contract in ADR-016/ADR-023 and the agent-facing tool description are untouched.

---

## Per-fork decisions

### D1 — Mechanism for cross-op atomicity

**Decision: two-phase — async prepare (materialize per-op write plans outside any transaction),
then one synchronous `atomic_unit` that applies all plans as DML with a per-op SAVEPOINT.**

The atomic path never dispatches a full verb handler inside `atomic_unit`. Instead:

1. **Prepare pass (async, per op, outside any transaction).** Each admissible op runs its verb's
   `prepare` step — the same validation, identifier generation, secret masking, and (for verbs
   that require it) embedding computation the handler does today — and produces a materialized
   **write plan**: the concrete set of `SqlStatement`s that op will apply (base rows, FTS rows,
   and any precomputed vector rows), plus a record of any post-commit side effect (e.g. ANN warm)
   to run after the transaction. The ops-file is independent ops (no `$prev`), so prepare never
   needs to _reference_ another op's output — but prepare-time validations can still be
   invalidated by an earlier op in the same file naming the same caller-known identifier; plans
   therefore carry in-transaction guards, per the validation-staleness rules below.
2. **Commit pass (synchronous, one `atomic_unit`).** The runner opens one `atomic_unit`; inside
   the closure — which drives only the `InlineWriter` and therefore never suspends — it applies
   each op's plan under a named SAVEPOINT (`op_<n>`). On plan success the SAVEPOINT is released; on
   the first plan error the whole unit rolls back and the failing op index is recorded. The unit
   commits all plans or none.
3. **Post-commit pass (async).** Deferred side effects recorded in prepare (ANN warming / index
   maintenance) run once, after the commit, outside the transaction.

The commit-pass closure is, by construction, DML-only, so it satisfies the _atomic-unit
suspend-free invariant_: behavior is identical whether or not the single-writer flag is on. This
is the whole reason the previous draft's "dispatch real handlers inside `atomic_unit`" mechanism
was rejected — it violated the invariant. Two-phase relocates every suspension-bearing step
(embedding, ANN) to the prepare and post-commit passes, leaving only synchronous DML under the
transaction. It also keeps the SQLite write transaction open only for the DML apply, never across
embedding or ANN work, which is what makes the writer-hold and daemon-coexistence bounds (D2,
"Daemon coexistence") honest.

The handler-logic-duplication objection is answered by **refactoring** each admissible verb to
expose its own `prepare` → apply seam, reusing the handler's existing compute and statement
generation. The bulk-apply crate calls that seam; it does not re-derive identifier generation,
validation, or index maintenance. This is the same "transaction-enlisted execute-only" pattern the
codebase already uses where a durable append must enlist in a caller-owned transaction: expose a
step that runs statements on a provided writer and opens no transaction of its own, and let the
caller wrap N of them in one unit.

**This two-phase shape is the chosen mechanism. Its two known weak points, examined rather than
assumed away:**

- **Prepare/commit staleness.** In two-phase, an op's write plan is materialized in the prepare
  pass, before the transaction opens. Staleness therefore has two distinct classes, and they are
  handled differently:

  **Content-derived staleness** (a precomputed value goes stale): if a _later_ op in the same file
  mutates the same target, an earlier op's precomputed value can be committed against content the
  later op overwrote — e.g. an embedding computed for content C, committed while a later op sets
  the content to C'. **v1 is immune to this class:** the v1 admissible set is DML-only (D3), so
  the prepare pass computes no embeddings and materializes only parameterized statements; there is
  nothing content-derived to go stale. The hazard exists **only** for the deferred
  embedding-bearing extension, and it is a hard precondition on that extension: before any
  embedding-bearing verb is admitted, the atomic runner must detect intra-file write-write
  conflicts on the same target (reject, or recompute the loser's embedding in target order).

  **Validation staleness** (a prepare-time validation goes stale): prepare validates each op
  against **pre-transaction** state, but ops in one file can name the same caller-known
  identifier, so an earlier op in the commit pass can invalidate what a later op's prepare
  checked. This class is NOT excluded by DML-only scoping, and `graph_edges` has no foreign-key
  enforcement (endpoint integrity is handler-validation only), so SQLite will happily commit the
  inconsistency. Concrete v1 hazards: a file `[delete(X, hard), link(A, X)]` would commit a
  dangling edge; `[link(from, Z), merge(into, from)]` would strand a live edge on the merged-away
  entity if merge's plan pre-enumerated edge rows at prepare; `[delete(X), update(X)]` would
  report a committed success whose UPDATE affected zero rows. **v1 closes this class with two
  mandatory plan rules:**

  1. **Predicate-based plans wherever a write's scope depends on current state.** A plan whose
     effect covers "all rows matching a condition" must be materialized as a predicated statement
     evaluated **inside** the transaction (e.g. merge's edge rewire is
     `UPDATE graph_edges SET source_id = :into WHERE source_id = :from`), never as a
     prepare-time-enumerated row list. In-transaction evaluation sees every earlier op's writes,
     so this rule eliminates the merge-rewire hazard structurally.
  2. **Affected-row guards wherever prepare assumed row existence.** Any plan statement whose
     prepare-time validation asserted that a target row exists (an edge insert's endpoint check, an
     update's target check, a delete's target check) carries an expected-effect guard checked in
     apply: if the affected-row count (or an in-transaction existence probe for inserts whose
     validity depends on another row) does not match what prepare assumed, the op **fails inside
     the unit** and the whole unit rolls back. A prepare-time validation is thus a plan
     _hypothesis_, re-verified under the transaction, never a commitment.

  The immunity claim is deliberately narrow: DML-only scoping buys immunity to **content-derived**
  staleness only; **validation** staleness is closed by the two plan rules above, and the
  dangling-edge acceptance criterion pins the worst case. (A conservative alternative — rejecting
  any `--atomic` file with two ops naming the same target — was considered and rejected for v1:
  same-target sequences like transition-then-complete are legitimate, and the guard rules make
  them safe rather than forbidden.)
- **Refactoring surface of "the handler's own factored code."** Each admissible verb must grow a
  `prepare`/apply split. For the v1 DML-only set the split is small: those handlers already reduce
  to "validate and resolve identifiers, then execute statements," so `prepare` returns the
  statements and `apply` runs them. The cost is real but bounded to the v1 verb list, and it is a
  factoring of existing code, not new logic. The embedding-bearing extension is where the split
  becomes a genuine per-handler refactor (hoisting embedding out of the write), which is the second
  reason it is deferred out of v1.

Neither weak point sinks two-phase; both are contained by scoping v1 to DML-only verbs, and both
are stated as explicit gates on the extension. That is why two-phase is chosen over the rejected
alternatives below.

**Alternatives considered:**

- **(a) Dispatch full verb handlers inside one `atomic_unit` (the previous draft's mechanism).**
  Rejected — unsupported by the shipped seam. Real handlers suspend (embedding, ANN warm);
  `atomic_unit`'s flag-on driver (`block_on_sync`) returns an error on suspension, and the flag-off
  path would silently permit it, producing liveness-dependent behavior. This is the blocker that
  forced this redesign.
- **(b) Redesign `atomic_unit`/`WriterTask` to drive async closures on the writer connection.**
  Rejected. Allowing the writer task to `await` external work while holding the single write
  connection reintroduces exactly the long-held-write-lock starvation ADR-067 eliminated: an
  embedding call inside the held transaction would pin the write lock for the embedding latency.
  It also enlarges the most safety-critical seam in the storage layer for a CLI feature.
- **(c) Compile ops to raw SQL directly in the bulk-apply crate, bypassing handlers.** Rejected.
  It duplicates handler logic (identifier generation, dedup, endpoint validation, FTS/vector index
  maintenance, secret masking) and guarantees drift. The prepare/apply seam gives the same
  synchronous-DML result while reusing the handler's own code.
- **(d) Unbounded writer lease across arbitrary dispatch.** Rejected — the general lease holds the
  writer's transaction open across arbitrary async dispatch, the wedge shape ADR-067 removed. The
  two-phase design is the bounded, DML-only realization of the same intent.
- **(e) Savepoint-per-op.** Adopted as a component of the chosen design (the per-op SAVEPOINT in
  the commit pass), not an alternative to it.

### D2 — Unit of atomicity for `--ops-file`

**Decision: whole-file, opt-in via `--atomic`, default off (per-op, unchanged). Bounded by a
configurable op-count guard.**

- **Whole-file, not per-chunk.** The 100-op chunk is a transport-framing constant; all-or-nothing
  across a chunk boundary is not a semantically meaningful unit. A cleansing file is atomic as a
  whole or not at all.
- **Opt-in.** `--atomic` is off by default. Idempotent mass updates keep today's non-atomic,
  per-op-independent behavior with no latency change.
- **Bounded.** The commit pass holds the single writer for the duration of the DML apply. Because
  embedding and ANN work are hoisted out of the transaction (D1), the hold is a clean, near-linear
  function of the plan's DML statement count — no embedding latency inside the window. Even so, an
  unbounded 10k-op atomic file is a multi-second exclusive DML hold and a product-visible stall for
  every other writer on the process. Under `--atomic`, a file exceeding a configurable maximum op
  count (recommended default on the order of a few thousand ops; final value tuned against the
  load harness) is **rejected before execution** with a clear error directing the caller to split
  the file or run without `--atomic`.

**Alternatives considered:**

- **Per-chunk atomicity (all-or-nothing per 100).** Rejected: the boundary is arbitrary and gives
  the caller no guarantee they can reason about.
- **Always atomic (no flag).** Rejected: it would impose the whole-file writer-hold and the verb
  restriction on the common idempotent-update case and break callers relying on partial progress.

### D3 — Admissible verbs under `--atomic`

**Decision: admit only verbs that expose a prepare/apply seam whose in-transaction phase is
synchronous DML. The v1 admissible set is the DML-only mutations; embedding-bearing verbs and all
read verbs are rejected at parse time.**

- **v1 admissible:** `update`, `delete`, `link`, `merge`, and the task-lifecycle and message
  mutations (`gtd.*` transitions, `comm` mutations), plus the governance verbs
  (`propose`/`review`/`withdraw`) — the verbs whose prepare is validation and identifier
  resolution only and whose apply is pure DML.
- **`update` and `merge` caveat (verified at source).** Under the current handlers, `update`
  triggers a reindex when name or description change, and that reindex awaits embedding
  (`reindex_entity` in `crates/khive-runtime/src/curation.rs`); property-only updates skip
  reindex entirely (covered by an existing regression test). `merge` already performs its
  vector re-insert **after** its transaction, precisely because embedding is async — the
  handler's own documentation states this. So the codebase already contains the pattern this
  design needs: row and FTS DML commit inside the transaction; embedding reindex runs as a
  post-commit side effect, under reindex's existing best-effort warn-and-continue contract (a
  failed re-embed leaves a stale vector, never a failed write). Under `--atomic`, `update` and
  `merge` follow that same split: their write plans carry the row/FTS DML; any needed reindex
  is recorded as a post-commit side effect and computes its embedding from the **committed**
  row content, which also means no prepare-time embedding exists to go stale. The v1 prepare
  pass therefore still computes no embeddings, and the staleness-immunity claim in D1 holds.
- **v1 rejected — embedding-bearing:** `create` (entity/note/document) and `memory.remember`
  compute embeddings and warm ANN as part of the write. They are excluded from `--atomic` v1
  because their prepare seam (hoisting embedding out of the transaction) is not yet built. An
  `--atomic` file containing one is rejected before execution with an error naming the line and
  verb and stating that embedding-bearing verbs are not yet atomic-eligible.
- **v1 rejected — reads:** `search` / `recall` / `query` / `traverse` / `list` / `get` /
  `neighbors` / `context` and the other read verbs are rejected: they do not belong in an
  all-or-nothing write unit, and (per the prepare/apply model) they have no write plan to apply.

The admissibility of a verb is a static property — does its apply phase honor the _atomic-unit
suspend-free invariant_ (reduce to synchronous DML with no in-transaction await)? — declared per
verb as pack metadata. The parser consults that metadata and rejects any inadmissible op before
the prepare pass runs. This is additive pack metadata, not a change to handler signatures. The
admissibility flag is the design-time expression of the same invariant the suspend-trap regression
test enforces at runtime (Acceptance criteria).

**Extension path (not v1):** embedding-bearing verbs become admissible once their handlers hoist
embedding computation into the prepare pass so the transaction still applies only synchronous DML.
This is deliberately deferred: it is a larger per-handler refactor, and the #195 driver
(salience recalibration, mass cleanse) is satisfied by the DML-only set.

**Alternative considered: admit all write verbs and hold the transaction across their embedding.**
Rejected: it holds the write lock across embedding/ANN latency, making the writer-hold and
daemon-coexistence bounds dishonest, and it conflicts with the `atomic_unit` suspension contract.

### Daemon coexistence under `--atomic`

Bulk apply runs in-process and does not traverse the daemon. When a live daemon is serving the same
database file, an `--atomic` run holds a **cross-process** `BEGIN IMMEDIATE` on that file for the
duration of the commit pass, while the daemon's write owner operates inside its own process. The
single-writer guarantee is per-process; the two processes coordinate through SQLite's file-level
write lock.

**Decision: accept bounded cross-process coexistence; do not route `--atomic` through the daemon.**

- This coexistence is not new. The non-atomic bulk apply already writes cross-process against a
  live daemon today — each op takes its own short-lived write lock. `--atomic` changes the
  **duration** of the hold, not its existence, and the duration is exactly what the D2 op-count
  guard bounds. Because embedding is hoisted out of the transaction (D1), the held window is
  DML-only, so the guard's op-count-to-duration relationship is clean and the bound is honest.
- Expected daemon-side behavior during the window: daemon writes wait on the configured
  `busy_timeout` and proceed when the unit commits. A window shorter than the busy timeout delays
  daemon writes without failing them; a window that exceeds it surfaces `SQLITE_BUSY` to daemon
  callers. The op-count guard default is therefore sized (from harness measurement) so the
  worst-case DML hold stays well inside the default busy timeout, and the `--atomic` documentation
  directs operators to run large atomic cleanses in a maintenance window or against an idle daemon.
- **Alternative considered: route `--atomic` through the daemon when one is live.** Rejected for
  this ADR. It requires a new daemon bulk-transaction protocol surface, reintroduces the transport
  frame cap that in-process bulk apply exists to avoid, and creates a behavioral fork (semantics
  differ by daemon liveness). If operational experience shows the bounded window is insufficient,
  daemon-routed atomicity can be a follow-up.
- **Alternative considered: refuse `--atomic` when a live daemon is detected.** Rejected: liveness
  detection is racy and a hard refusal blocks the legitimate bounded case the guard covers.

### D4 — Error response shape for partial-failure rollback

**Decision: preserve the existing `results` + `summary` shape and add an `atomic` block. Pin the
field names.**

The non-atomic (default) CLI output is **unchanged**. The `--atomic` output is the same
`results` + `summary` shape plus one additive top-level `atomic` object:

```json
{
  "results": [ { "ok": true,  "tool": "update", "result": { ... } },
               { "ok": false, "tool": "delete", "error": "..." },
               { "ok": false, "tool": "update", "error": "unit rolled back" } ],
  "summary": { "total": 3, "succeeded": 0, "failed": 3 },
  "atomic":  { "committed": false, "rolled_back": true, "failed_op_index": 1,
               "error": "<message from the failing op>" }
}
```

- On full success: `"atomic": { "committed": true, "rolled_back": false, "failed_op_index": null }`,
  and `results`/`summary` carry the per-op outcomes as usual.
- On abort: `committed=false`, `rolled_back=true`, `failed_op_index` = the zero-based index of the
  first failing op, `error` = that op's error message. Because the unit rolled back, no op is
  reported as `succeeded` in `summary`; each entry in `results` reflects not-committed. The
  failing op's `results` entry carries its real error; the others carry a uniform "unit rolled
  back" marker so a reader can distinguish the cause from the collateral.
- The `atomic` object appears only on the `--atomic` path. `results` and `summary` retain the exact
  meaning ADR-016 defines; no field is removed or repurposed. This is the `kkernel exec`
  CLI-printed shape and does not alter the MCP `request` wire envelope.

**Alternative considered: a separate `ops` array instead of `results`.** Rejected: it diverges from
the established `results`/`summary` contract for no benefit; the additive `atomic` block carries
everything the atomic path needs.

### D5 — Fate of `begin_tx()`

**Decision: convert the session-ingest caller to `atomic_unit`, closing that standalone-writer
hole; then remove the `begin_tx` trait method if no non-test caller remains.**

The session-mirror ingest path (`write_events_and_cursor`) is a clean `atomic_unit` shape: it
performs a sequence of DML statements (session upsert, message insert, cursor advance) and commits
once, with no branch-on-read logic that needs a live incremental handle. It converts by moving the
loop body into the `atomic_unit` closure; the cursor advance is the closure's last statement; the
all-or-nothing contract is preserved because `atomic_unit` commits once or rolls the whole closure
back.

**Suspension-safety (verified — this closure must satisfy the _atomic-unit suspend-free
invariant_).** The converted closure drives **only** the provided writer with inline-built
`SqlStatement`s: it issues session and message INSERTs and the cursor UPDATE and does no embedding,
no ANN warming, and no other `await` on an external service. On the single-writer path the closure
therefore resolves on its first poll and satisfies the invariant; on the flag-off path it runs
under the manual transaction identically. The conversion is admissible precisely because the ingest
write is already pure sequential DML. The implementation must keep it that way: no embedding or
service call may be introduced inside the closure, and the acceptance suite includes a test that
the closure does not suspend.

Once converted, `begin_tx` has no live production caller **in this ADR's scope**. The hole is closed
by removing the caller, independent of whether the method is deleted. Implementation then:

1. Greps for any remaining non-test caller of `begin_tx` and of the `SqlTransaction` trait.
2. If none remains and `SqlTransaction` is not load-bearing forward-deployed infrastructure,
   removes `begin_tx` from `SqlAccess` and its `SqlBridge` implementation and collapses the dead
   machinery.
3. If `SqlTransaction` is genuinely forward-deployed infrastructure, keeps the trait but leaves
   `begin_tx` with no production caller and a doc note that production atomicity goes through
   `atomic_unit`. Either way the competing-writer hole from this path is closed.

Removing a method from `SqlAccess` touches the storage trait surface (declaration, `SqlBridge`
implementation, test callers). The trait is internal to the runtime; no external SDK consumes it,
so the change is contained.

**Alternatives considered:**

- **(i) Migrate `begin_tx` onto the writer task as a lease.** Rejected: a live incremental handle
  held across caller-driven calls is the long-held-lock shape ADR-067 removed, and the one live
  caller needs one atomic closure, which `atomic_unit` already provides.
- **(iii) Keep `begin_tx` standalone as a permanent exemption.** Rejected: as long as a live caller
  opens a standalone `BEGIN IMMEDIATE` outside the writer, the single-writer guarantee has a hole.
  `atomic_unit` is the seam to move the caller to.

### D6 — Document structure

**Decision: confirm the split.** This follow-up ADR carries the design (D1–D5). A **separate,
small ADR-067 amendment** records the now-final write-path facts as merged: the session-ingest
`begin_tx` conversion, the `orphan_sweep` residual and its separately-tracked conversion, and the
top-level maintenance seam (`execute_script_top_level`) as shipped.

Rationale: the amendment is a documentation-truth correction to an already-accepted ADR and can
land independently; the design ADR is a forward decision that goes through the design-review gate.
Keeping them separate matches the repository convention of not bundling a documentation correction
with a design change, and lets neither block the other. `orphan_sweep`'s conversion is tracked
under that amendment as its own change and is **not** absorbed into this ADR's migration steps.

**Alternative considered: one combined document.** Rejected: it couples an immediately-landable
inventory correction to a design that needs review.

---

## Migration steps

0. **Warn the seam in its own code.** Add a doc-comment to `SqlAccess::atomic_unit` (the trait
   declaration in `crates/khive-storage/src/sql.rs`) and to its `SqlBridge` implementation
   (`crates/khive-db/src/sql_bridge.rs`) stating the _atomic-unit suspend-free invariant_
   explicitly: the closure must complete on its first poll and may issue only synchronous DML; any
   real `await` (embedding, ANN, service, channel) fails on the single-writer path and silently
   passes flag-off. This near-shipped twice in one day; the warning must live in the code the next
   consumer reads, not only in this ADR.
1. Add a per-verb `prepare`/apply seam to the v1 admissible verbs (`update`, `delete`, `link`,
   `merge`, the task-lifecycle and message mutations, the governance verbs): `prepare` runs the
   existing async validation/identifier work and returns a materialized write plan (the
   `SqlStatement`s plus any post-commit side effect); apply issues those statements on a provided
   writer with no transaction control of its own.
2. Add per-verb atomic-admissibility metadata and the parse-time rejection for inadmissible verbs
   (embedding-bearing and read verbs) under `--atomic`.
3. Add the atomic runner: an in-process entry that runs the prepare pass over the file, opens one
   `atomic_unit`, applies each plan under a per-op SAVEPOINT, commits or rolls back, then runs the
   post-commit side effects.
4. Add `--atomic` to `exec --ops-file`: enforce the op-count guard (D2), route the file through the
   atomic runner, and print the additive `atomic` envelope (D4).
5. Convert `write_events_and_cursor` (session-mirror ingest) from `begin_tx` to `atomic_unit`
   (D5), preserving the cursor-advances-only-on-commit contract and its suspension-free property.
6. Grep for remaining non-test `begin_tx`/`SqlTransaction` callers; remove `begin_tx` from
   `SqlAccess` and its implementation if none remains, else document the no-live-caller state.
7. Land the companion ADR-067 amendment separately (D6). `orphan_sweep` conversion is tracked
   there, not here.

Schema is unchanged; no migration file is required. Pack rules gain only additive per-verb metadata.

---

## Acceptance criteria

- **Atomic rollback end-to-end.** An `--atomic` ops-file of admissible verbs with a deliberate
  mid-file failure leaves the database **unchanged**: none of the file's writes are present after
  the run, and the response reports `atomic.committed=false` with `failed_op_index` set to the
  first failing op.
- **Suspend-trap regression test (fails loudly, never silently).** Two paired assertions enforce
  the _atomic-unit suspend-free invariant_: (a) the real commit-pass closure over admissible DML
  verbs resolves on its first poll and commits, proving the happy path is genuinely synchronous;
  and (b) a closure that deliberately routes a _suspending_ future (a real embedding-bearing
  handler, or a stand-in that awaits) through the atomic path returns the `block_on_sync`
  suspension error and aborts the unit — it must fail loudly, not silently succeed or wedge. This
  is the guard that a future change admitting an embedding-bearing verb without hoisting its
  embedding is caught by a test, not discovered in production on the single-writer path.
- **Embedding-bearing verb rejected.** An `--atomic` file containing `create` or `memory.remember`
  is rejected before execution, naming the line and verb and stating embedding-bearing verbs are
  not yet atomic-eligible.
- **Read verb rejected.** An `--atomic` file containing a read/recall/query verb is rejected before
  any write occurs, naming the line and verb.
- **Op-count guard.** An `--atomic` file exceeding the configured maximum op count is rejected
  before execution with an actionable error.
- **No dangling edge from intra-file validation staleness.** An `--atomic` file of the form
  `[delete(X, hard), link(A, X)]` must **not** commit a dangling edge: the run is either rejected
  up front or the unit rolls back in-transaction (the link op's affected-row/existence guard fails
  once X is gone). After the run, the database contains neither the edge nor a partial subset of
  the file's writes.
- **Non-atomic parity.** With `--atomic` absent, bulk apply output and per-op semantics are
  byte-identical to the pre-change behavior (per-op independence preserved; a failing op does not
  abort siblings).
- **Envelope contract.** On both success and abort, the `--atomic` output preserves `results` and
  `summary` per ADR-016 and adds the `atomic` block with the pinned field names (D4).
- **Session-ingest suspension-free.** A test asserts the converted `write_events_and_cursor`
  closure does not suspend (drives only synchronous DML), so it is `block_on_sync`-safe on the
  single-writer path.
- **Single-writer concurrency test (mandatory).** Concurrent session-mirror ingest plus normal
  write traffic, with the single-writer task active, shows **no standalone `BEGIN IMMEDIATE` from
  the session-ingest path** outside the writer owner after conversion. This path is not exercised
  by the general write-load harness and must be covered explicitly. (This criterion is scoped to
  the session-ingest path; `orphan_sweep` is out of scope, tracked under the ADR-067 amendment.)
- **Daemon coexistence under `--atomic`.** With a live daemon serving the same database file and
  carrying concurrent write traffic, an `--atomic` run within the op-count guard commits
  successfully, the daemon's concurrent writes complete (delayed at most by the window, none lost,
  no corruption), and any daemon-side `SQLITE_BUSY` is observed only if the window is deliberately
  driven past the busy timeout. This cross-process case is covered by neither the general
  write-load harness nor the in-process suite and must be exercised explicitly.
- **Revert-companion test.** The key session-ingest and atomic-rollback tests demonstrably **fail**
  against the pre-change shape (standalone `begin_tx`; per-op non-atomic bulk apply), proving the
  tests are non-vacuous.
- **Session-ingest no-partial-state preserved.** The existing regression that asserts the mirror
  cursor advances only when row writes commit passes after the `atomic_unit` conversion.

---

## Open questions

1. **v1 admissible-set boundary — resolved at source.** `update` does embed on a name or
   description change (via `reindex_entity`), and `merge` reindexes as well; property-only
   updates skip reindex. Resolution adopted in D3: the reindex embedding moves to the
   post-commit pass, following the pattern `merge` already uses today (its vector re-insert
   runs after its transaction) and reindex's existing best-effort contract. Migration step 1
   implements this split per verb; any verb whose embedding cannot be deferred post-commit is
   moved to the deferred embedding-bearing set instead.
2. **Op-count guard default.** The exact maximum-ops threshold for `--atomic` should be set from a
   harness measurement of DML-only writer-hold time versus op count, not guessed. Ships configurable
   with a conservative default.
3. **`SqlTransaction` retention.** Whether the `SqlTransaction` trait is forward-deployed
   infrastructure worth keeping after `begin_tx`'s last caller is gone, or dead machinery to
   collapse. Resolved by the implementation-time grep in migration step 6.

---

## References

- ADR-067 — Write-Owner Daemon: single-writer task, the `atomic_unit` seam and its synchronous-DML
  (`block_on_sync`) contract, per-request SAVEPOINT isolation, and the `--ops-file` cross-op
  atomicity deferral this ADR resolves.
- ADR-005 — Storage capability traits: `SqlAccess`, `atomic_unit`, `begin_tx`, `SqlTransaction`.
- ADR-016 — Request DSL and the `results`/`summary` response contract this ADR preserves and
  extends on the CLI atomic path.
- Issue #195 — true cross-op atomicity for `exec --ops-file` bulk apply.
