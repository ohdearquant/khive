# Atomic Prepare — DML Plan Builders

`atomic_prepare.rs` turns a subset of verb calls into a static, guarded DML plan
(`crate::atomic_plan::PlanStatement`s) that the `--atomic` execution path can run inside one
transaction without invoking the normal async handler. This document covers which verbs are
in scope, why some are deliberately excluded, and the DML-shape parity guarantees for the
functions that build those plans.

Pack-owned mutation invariants are adapted at the `kkernel` boundary, where both
the `VerbRegistry` and this runtime plan vocabulary are visible. For task note
updates and task dependency links, `kkernel` invokes the same `KindHook` update
normalizer/validators as canonical KG dispatch before building the substrate plan.
For a note update, the hook and plan consume one shared note snapshot; the generated
statement compares that snapshot's `updated_at` and deletion marker at apply time.
Concurrent changes and repeated same-target note updates prepared without projected
state therefore fail their affected-row guard and roll back the unit.
Core migration V15's
transaction-time triggers remain the final backstop: they see earlier statements
in the same atomic unit and close concurrent check/write races that an async prepare
pass cannot.

## Scope: what is excluded and why

`gtd.transition` / `gtd.complete` prepare is deliberately **not** here: their lifecycle
vocabulary (`is_terminal`, `can_transition`, ...) lives in `khive-pack-gtd`, which depends on
`khive-runtime` — not the other way around. Reproducing that dependency here would invert the
crate graph, so their prepare functions live in `kkernel` (which already depends on both crates),
calling back into the plain `PlanStatement`/`AffectedRowGuard` shapes exported from this module's
sibling, `crate::atomic_plan`.

`propose` / `review` / `withdraw` (the event-sourced governance lifecycle) are on the v1
admissible list (`khive_types::pack::ATOMIC_ADMISSIBLE_VERBS`) but have no prepare implementation
here: their apply path is a changeset-interpreter (`apply_worker`) over a dedicated
`proposals_open` table, not a small number of guarded DML statements — a faithful, non-stub
atomic prepare for them is separate follow-on work. `prepare_governance_unimplemented` fails
loudly, before any write, naming this as a known scope gap rather than silently no-opping.

`merge` is likewise on the v1 admissible list but is deferred: full-parity field folding,
survivor index reindex, loser index purge, provenance, and same-kind rejection are achievable as
static DML, but `curation::merge_entity_sql`'s graceful edge-conflict resolution is not (it is
per-row procedural, incompatible with the static predicate/guard plan shape): rather than ship a
partially-scoped atomic merge, it is rejected at the same pre-runtime static guard as governance
(`khive_types::pack::ATOMIC_KNOWN_UNIMPLEMENTED_VERBS`). `prepare_merge` is therefore unreachable
through `--atomic`; it remains only as the earlier direct-prepare implementation, exercised by
this module's own tests, and as defense in depth.

## prepare_update_edge

Mirrors `khive-runtime::operations::KhiveRuntime::update_edge`'s patch semantics exactly:
`relation`/`weight`/`properties` are the only applicable fields
(`reject_inapplicable_update_fields`'s `"edge"` arm enforces this before any mutation), a changed
`relation` is endpoint-validated first, `weight` is range-checked, and `properties` REPLACES
`metadata` wholesale (no merge — `update_edge` does `edge.metadata = Some(props)`, unlike the
entity/note branches' `merge_properties`).

DML shape:

- non-symmetric relation: a single guarded `edge_replace_if_unchanged_statement` call on the
  patched `Edge`, carrying an `AffectedRowGuard::exactly(1)` — the same CAS shape `update_edge`'s
  own non-symmetric branch runs via `graph.replace_edge_if_unchanged(edge.clone(), expected_updated_at,
  expected_deleted_at)` (`khive-db::stores::graph::SqlGraphStore::replace_edge_if_unchanged`). Both
  sides bind the fetched snapshot's `updated_at`/`deleted_at` as the CAS fence and advance
  `updated_at` to `max(now, snapshot + 1µs)` so the replacement revision strictly increases even
  inside one clock microsecond; zero affected rows means a concurrent writer moved the edge between
  read and write, and the caller must roll back rather than silently overwrite it.
- symmetric relation (`competes_with`, `composed_with`): neither side uses the upsert builder here,
  because `upsert_edge` resolves `ON CONFLICT(namespace, id)` first and cannot detect a natural-key
  collision with a _different_ id. Canonical (`update_edge_symmetric_dml`) runs a conflict probe and
  branches in Rust inside a single uninterrupted transaction, which is safe there. This atomic path
  cannot do that (see the in-source invariant note on `prepare_update_edge`), so it always emits
  both `edge_symmetric_delete_if_conflict_statement` and
  `edge_symmetric_absorb_or_update_inplace_statement`, each carrying its own commit-time predicate
  bound to the fetched snapshot's `updated_at`/`deleted_at`: a conflicting canonical survivor alone
  is not sufficient grounds to delete the non-canonical row if that row changed since the snapshot
  was read.

## event_append_statements

Builds the `Event` exactly as each canonical site does and turns it into plain-data
`SqlStatement`s via `khive_db::stores::event::event_insert_statements`: the same builder the async
execution path every canonical `event_store.append_event(...)` call reaches uses. There is exactly
one place that knows the `events`/`event_observations` insert shape; this function only adapts its
output into unguarded `PlanStatement`s for the atomic-unit plan.

This is a `PlanStatement` inside the atomic unit, not a `PostCommitEffect` (reserved for
best-effort or non-SQL work): the insert is a small number of plain, deterministic `INSERT`s
computed entirely from data already on hand at prepare time, unlike the
`ReindexEntity`/`ReindexNote` post-commit effects this module defers because those need an
embedding call.

`apply_post_commit_effects_with_report` returns one `PostCommitEmbeddingOutcome` for every reindex
effect it successfully executes. Each outcome retains the originating effect identity plus its
typed truncation report, allowing CLI or pack response builders to attach warnings to the exact
write without rerunning registry prediction. The original `apply_post_commit_effects` remains the
unit-returning compatibility wrapper.
