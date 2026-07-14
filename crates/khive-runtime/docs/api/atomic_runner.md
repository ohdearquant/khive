# Atomic Runner — Commit-Pass Mechanism

`atomic_runner.rs` is the synchronous commit-pass mechanism for the ADR-099 atomic-write
migration: given an already-prepared sequence of write plans, it applies them as one
`SqlAccess::atomic_unit`, under a per-op `SAVEPOINT`, committing every plan or rolling back the
whole unit. This document covers the suspend-free safety argument, the three-phase shape the
module is one piece of, and its current wiring status.

## Wiring status (ADR-099 — B3 shipped)

This module is the synchronous commit-pass piece of the shipped ADR-099 B3 flow. The ADR-099 B3
caller is `kkernel exec --ops-file --atomic` (see `kkernel`'s `atomic_apply` module): it runs the
parse-time admissibility check (B1), drives the async prepare pass (ops → `AtomicOpPlan`), calls
`run_atomic_unit` here for the one synchronous commit pass (B2), then applies the async
post-commit effects (reindex). Only the currently executable verb set (`update`, `delete`,
`link`, `gtd.transition`, `gtd.complete`) may appear in an atomic ops-file. `merge` and the
governance verbs (`propose`, `review`, `withdraw`) remain conceptually admissible under
ADR-099 D3 but are rejected up front as known-unimplemented (`ATOMIC_KNOWN_UNIMPLEMENTED_VERBS`
in `khive-types`) — no full-parity prepare/apply seam exists for them yet. Embedding-bearing,
read, or unlisted verbs are likewise rejected before any write.

The CLI is not the runner's sole production consumer: runtime callers can also supply prepared
plans directly — the hard-delete path (`atomic_hard_delete_with_edge_purge` in `operations.rs`)
constructs a `DeletePlan` and invokes `run_atomic_unit` for its row-delete + incident-edge purge.
Tests construct plans directly as well.

## Suspend-free invariant

`run_atomic_unit` is the one place in this crate that builds an `AtomicUnitOp` closure and hands
it to `SqlAccess::atomic_unit`. That trait method carries a hard contract (its own doc comment,
`crates/khive-storage/src/sql.rs`, and `crates/khive-db/src/sql_bridge.rs` `block_on_sync`):
**the closure's future must resolve on its first poll** — synchronous DML against the provided
`&mut dyn SqlWriter` only, never a real `.await` on embedding, ANN warming, or any other
suspending work.

This module honors that invariant structurally, not by convention: every statement the
commit-pass closure drives comes from `AtomicOpPlan::plan_statements` (private — the runner's own
internal flattening step), which can only ever produce `PlanStatement`s — plain parameterized
SQL, the same shape ADR-099 D1's prepare pass produces for the v1 DML-only admissible verb set
(ADR-099 D3). There is no code path in this module that can hand `atomic_unit` an embedding call
or any other suspending future.

The paired suspend-trap tests at the bottom of the file check the two things this promise rests
on: the real commit pass resolving on first poll (the happy-path proof), and a hand-built closure
that deliberately suspends failing loudly through the exact same seam (the misuse-is-caught
proof).

## Two-phase shape (ADR-099 D1)

Only the **commit pass** (phase 2) lives here: given an already-prepared `Vec<AtomicOpPlan>`
(phase 1, the async prepare pass, is out of scope for B2), `run_atomic_unit` opens one
`atomic_unit`, applies each plan's statements under a
named `SAVEPOINT`, and returns either every op's collected `PostCommitEffect`s (phase 3, the
async post-commit pass — the B3 wiring point: nothing in B2 executes these effects, a test
consumer only drains the returned list) or the first op's failure and its index.
