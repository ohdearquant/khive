# atomic_prepare.rs — extended rationale

Long-form rationale extracted from `crates/khive-runtime/src/atomic_prepare.rs` doc-comments
during the rustdoc condense pass.

## module-scope

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
- non-symmetric relation: a single `edge_upsert_statement` call on the patched `Edge` — the same
  builder `update_edge`'s own non-symmetric branch calls via `graph.upsert_edge(edge.clone())`
  (`khive-db::stores::graph::SqlGraphStore::upsert_edge`), so parity is exact by construction.
- symmetric relation (`competes_with`, `composed_with`): `update_edge` does NOT use the upsert
  builder here, because `upsert_edge` resolves `ON CONFLICT(namespace, id)` first and cannot
  detect a natural-key collision with a *different* id. Canonical (`update_edge_symmetric_dml`)
  runs a conflict probe and branches in Rust inside a single uninterrupted transaction, which is
  safe there. This atomic path cannot do that (see the in-source invariant note on
  `prepare_update_edge`).

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
