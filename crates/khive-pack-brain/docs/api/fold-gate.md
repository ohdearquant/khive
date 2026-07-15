# Bounded-mass fold gate for implicit feedback

Source: `crates/khive-pack-brain/src/fold_gate.rs` (ADR-081 §2). Governs how implicit
feedback events are folded into posteriors without letting unlimited implicit signal
outweigh a single explicit event.

## The mass invariant

At any instant, the decayed implicit feedback mass folded into a posterior for a given
accounting key `(profile_id, namespace, target_id)` never exceeds `IMPLICIT_MASS_CAP` —
the weight of one explicit event. An incoming implicit event folds at its full weight only
if `M(k) + w <= CAP`; otherwise it is recorded in the event log (audit preserved, per the
data-vs-view principle) and folded at zero weight.

`M(k) = sum(w_i * 2^(-dt_i / T))` is not recomputed from the raw event log on every fold.
It is maintained as a single-row-per-key materialized accumulator (`brain_implicit_mass`),
read-decayed-written on each gated event, mirroring the existing `brain_profile_snapshots`
pattern of a derived accumulator living alongside the append-only `brain_event_log`.

## Concurrency (ADR-081 §2, normative)

> "the mass check and the fold execute in one SQLite transaction opened with
> `BEGIN IMMEDIATE`... database-level single-writer semantics serialize every
> check-and-fold against all concurrent writers."

`apply_fold_gate` satisfies this by handing the whole check-and-fold — the mass `SELECT`,
the decision (computed in Rust from the row read inside the unit), and the
`INSERT ... ON CONFLICT ... DO UPDATE` — to `SqlAccess::atomic_unit` as ONE suspension-free
unit. The seam owns the transaction boundary: the unit runs under a single
`BEGIN IMMEDIATE` with commit-on-Ok / rollback-on-Err, on the writer task's connection when
the single-writer queue is enabled and on one held writer connection otherwise.

On a file-backed pool, that `BEGIN IMMEDIATE` acquires SQLite's actual file-level RESERVED
lock for the duration, which SQLite enforces **across processes**, not just within one —
the property production needs, since khive-mcp routinely runs multiple concurrent daemon
processes against the same database file (issue #407). An in-process mutex alone (e.g.
`dispatch_gate` in `BrainPack::dispatch`) cannot serialize the check-and-fold; only
SQLite's own write lock can. The concurrency proof
(`fold_gate_concurrent_writers_never_exceed_cap`) uses a real file-backed `KhiveRuntime`
because only the file-backed path exhibits genuine cross-connection contention — the same
shape production's multiple concurrent `kkernel mcp` processes have.

Historical note: this function originally issued `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK`
itself on a retained `writer()` handle; that shape nests inside the writer task's own
transaction under the single-writer queue and was converted to `atomic_unit`. The trait's
`begin_tx`/`SqlTransaction` surface it once avoided has since been retired entirely.

## Why the decay/clamp math runs in Rust, not SQL

SQL math functions (`pow`/`exp`/`ln`/`log`) are unavailable on this `rusqlite`/SQLite build
(verified empirically: `SELECT pow(2.0, -1.0)` raises "no such function"), which rules out
expressing the entire decayed-mass + clamp decision as one `INSERT ... RETURNING`
statement with the decay math inlined in SQL. The decay/clamp math instead runs in Rust
(`decayed_mass`, `gate_decision`, both pure and unit-tested) between the `BEGIN IMMEDIATE`
and the `INSERT`, reading only the row already fetched on the held connection — so no
other writer can observe or mutate that row between the read and the write.

## Scorer dedup (ADR-081 §2/§6)

A scorer-tagged event additionally claims a `(scorer_run_id, serve_ledger_id)` key in
`brain_scorer_dedup` via `INSERT OR IGNORE`, inside the SAME held `BEGIN IMMEDIATE`
transaction as the mass check-and-fold. A conflicting insert (0 rows affected) means a
prior call already claimed this exact pair, and this call returns `GateOutcome::Deduped`
before touching `brain_implicit_mass` at all — the claim and the fold commit or roll back
together, so a crash between them cannot leave a claimed-but-never-folded key.

Reading the ledger row's `scorer_run_id` column first (as `serve_ledger::resolve` does)
cannot provide this guarantee: two concurrent duplicate submissions both observe the
pre-backfill NULL and both pass, since the column is only backfilled after the fold
completes (`backfill_grade`, called from `handlers.rs` after event append) — that check is
a useful non-atomic fast path, not a correctness mechanism.

## `apply_fold_gate_and_append_event`: claim-rollback and forced-zero interaction

**Claim-conflict handling**: on a claim conflict, this returns `Deduped` before running the
fold or building/appending any event. If instead the claim succeeds but the event append
fails, the whole transaction — claim included — rolls back (the same commit/rollback shell
as `apply_fold_gate`), so a retry sees no claim and proceeds normally; the claim can never
outlive a failed event append the way it could when the event append ran in its own
separate transaction after this one committed.

**Forced-zero handling**: `FeedbackGateMode::ForcedZero` still runs the dedup claim step —
only the mass fold itself is skipped — so two concurrent forced-zero submissions for the
same `(scorer_run_id, serve_ledger_id)` pair can no longer both append a zero-weight audit
event.

## ADR-067 Component A: single `atomic_unit` closure, not a manually-owned transaction

Both `apply_fold_gate` and `apply_fold_gate_and_append_event` hand their whole
claim+check+fold(+append) unit to `SqlAccess::atomic_unit` as ONE closure, instead of
opening a `writer()` handle and issuing `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK` by hand. On
the flag-on path, `atomic_unit` runs the closure inside the writer task's single request
transaction — no separate connection competes for SQLite's write lock. On the flag-off (or
in-memory) path, `atomic_unit` wraps it in the same manual `BEGIN IMMEDIATE`/`COMMIT`/
`ROLLBACK` sequence these functions used to issue directly, so that path is unchanged.
