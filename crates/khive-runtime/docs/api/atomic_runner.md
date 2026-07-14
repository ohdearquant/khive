# Atomic Runner — Commit-Pass Mechanism

`atomic_runner.rs` is the synchronous commit-pass mechanism for the ADR-099 atomic-write
migration: given an already-prepared sequence of write plans, it applies them as one
`SqlAccess::atomic_unit`, under a per-op `SAVEPOINT`, committing every plan or rolling back the
whole unit. This document covers the suspend-free safety argument, the three-phase shape the
module is one piece of, and its current wiring status.

## Wiring status (ADR-099 migration step 3, sub-slice B2)

This module is the **mechanism only**. It has no production caller in B2 — no verb dispatch, no
CLI `--atomic` surface, no daemon wiring. Tests in this file are the only consumer. Wiring a real
per-verb `prepare` step (ops → `AtomicOpPlan`) and the `exec --ops-file --atomic` CLI surface is
ADR-099 migration steps 1 (cont'd) and 4 — referred to throughout the source as the **B3 wiring
point**.

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
(phase 1, the async prepare pass, is out of scope for B2 — a test-only caller constructs plans
directly), `run_atomic_unit` opens one `atomic_unit`, applies each plan's statements under a
named `SAVEPOINT`, and returns either every op's collected `PostCommitEffect`s (phase 3, the
async post-commit pass — the B3 wiring point: nothing in B2 executes these effects, a test
consumer only drains the returned list) or the first op's failure and its index.
