# Edge Conflict SQL Audit (ADR-099 B3)

Exhaustive workspace grep for every hand-written statement touching
`graph_edges`'s natural-key uniqueness (`UNIQUE(namespace, source_id,
target_id, relation)`) or symmetric-relation conflict resolution — i.e.
every site that could drift out of sync with the shared conflict-arm text.
Commands run:

```
grep -rn "ON CONFLICT" . --include="*.rs" --include="*.sql"
grep -rn "UPDATE graph_edges SET\|INSERT INTO graph_edges\|DELETE FROM graph_edges" . --include="*.rs"
grep -rn "target_backend = excluded.target_backend\|target_backend = ?" . --include="*.rs"
```

(paths below are relative to the crate workspace root, i.e. `crates/…`)

## Shared source-of-truth constants (the target every consumer below is measured against)

- `khive-db/src/stores/graph.rs:49` — `EDGE_NATURAL_KEY_CONFLICT_SET`: the
  `INSERT ... ON CONFLICT` SET-list text (weight/updated_at/deleted_at/
  metadata/target_backend).
- `khive-db/src/stores/graph.rs:214-228` — `EDGE_SYMMETRIC_CONFLICT_PROBE_SQL`
  / `EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL` /
  `EDGE_SYMMETRIC_REFRESH_CANONICAL_SQL` / `EDGE_SYMMETRIC_UPDATE_INPLACE_SQL`:
  the probe-then-branch DML canonical `update_edge_symmetric_dml` binds.

## Sites and disposition

| # | Site | Consumes shared builder/constant? | Disposition |
|---|------|-----------------------------------|-------------|
| 1 | `khive-db/src/stores/graph.rs:65-79` `edge_upsert_statement` | Yes — SQL text interpolates `{EDGE_NATURAL_KEY_CONFLICT_SET}` for both `ON CONFLICT` arms. | OK — this IS the source-of-truth builder. |
| 2 | `khive-db/src/stores/graph.rs:120-149` `edge_insert_guarded_by_endpoints_statement` (atomic guarded `link`) | Yes — same `{EDGE_NATURAL_KEY_CONFLICT_SET}` interpolation (previously fixed). | OK. |
| 3 | `khive-db/src/stores/graph.rs:850-877` `batch_upsert_edges` (backs `upsert_edges`/`link_many`) | **Fixed.** Previously hand-wrote the full `INSERT ... ON CONFLICT` (including both conflict arms) as an inline literal. Now calls `edge_upsert_statement(edge)` + `bind_params` per row — the SAME builder site 1 uses. | OK (previously a duplication site; now closed). |
| 4 | `khive-db/src/stores/graph.rs:214-263` `EDGE_SYMMETRIC_*_SQL` constants + their `edge_symmetric_*_statement` plan-shape builders | N/A — this IS the shared source of truth `operations.rs`'s canonical path binds directly. | OK. |
| 5 | `khive-runtime/src/operations.rs:3747-3830` `update_edge_symmetric_dml` (canonical, non-atomic `update_edge`'s symmetric branch) | Yes — binds `khive_db::stores::graph::EDGE_SYMMETRIC_CONFLICT_PROBE_SQL` / `EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL` / `EDGE_SYMMETRIC_REFRESH_CANONICAL_SQL` / `EDGE_SYMMETRIC_UPDATE_INPLACE_SQL` directly via `rusqlite::params!` (different binding mechanism than the async `SqlStatement`/`SqlValue` path, but the SQL TEXT is the shared constant). | OK — this is the documented control-group path; untouched. |
| 6 | `khive-db/src/stores/graph.rs:374-448` `edge_symmetric_delete_if_conflict_statement` / `edge_symmetric_refresh_or_update_inplace_statement` (atomic-only) | N/A — deliberately a DIFFERENT, self-guarding commit-time shape: the refresh statement now gates its natural-key arm on `changes() = 1` rather than reusing `EDGE_SYMMETRIC_REFRESH_CANONICAL_SQL`/`EDGE_SYMMETRIC_UPDATE_INPLACE_SQL` verbatim, because those two assume a single-transaction probe-then-branch that atomic's prepare/commit split cannot safely reproduce (see the doc comment immediately above these builders). | **Justified-distinct.** Structurally different requirement (commit-time-only, no prepare-time branch), not an accidental copy. |
| 7 | `khive-runtime/src/curation.rs:1082-1160` `merge_entity_sql`'s edge rewire (entity `merge`, case a/b) | **No.** Hand-writes `SELECT id FROM graph_edges WHERE namespace = ?1 AND source_id = ?2 AND target_id = ?3 AND relation = ?4 AND id != ?5` (line ~1110, byte-for-byte `EDGE_SYMMETRIC_CONFLICT_PROBE_SQL`), `DELETE FROM graph_edges WHERE namespace = ?1 AND id = ?2` (line ~1131, byte-for-byte `EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL`), and `UPDATE graph_edges SET weight = ?1, updated_at = ?2, deleted_at = NULL, target_backend = ?3, metadata = ?4 WHERE namespace = ?5 AND id = ?6` (line ~1135, byte-for-byte `EDGE_SYMMETRIC_REFRESH_CANONICAL_SQL`) as fresh string literals instead of the existing constants. | **Not justified — a real duplication gap, out of the current approved scope.** `merge` is not in ADR-099's v1 atomic-admissible set (deferred separately), so it was outside the reviewed diff, but it is the same drift class as the sites above. Flagging for follow-up rather than fixing here: this brief's approved scope is limited to `update`'s symmetric-write correctness (task 1) and `batch_upsert_edges` (task 2); touching `merge_entity_sql` was not requested and would expand the diff beyond what was scoped. |
| 8 | `khive-runtime/src/curation.rs:1538-1600` `merge_note_sql`'s edge rewire (note `merge`, case a/b) | **No.** Same three hand-copied literals as #7 (this is the note-merge sibling of the entity-merge rewire) — `SELECT id FROM graph_edges WHERE namespace = ...` conflict probe, `DELETE FROM graph_edges WHERE namespace = ?1 AND id = ?2`, and the `UPDATE graph_edges SET weight = ..., target_backend = ?3, metadata = ?4 ...` refresh, each retyped a second time from #7's already-duplicated text (so this is actually the *fourth* copy of the conflict-probe/refresh text in the workspace, counting the two canonical constants and #7). | Same disposition as #7 — real gap, flagged for follow-up, not fixed here (same out-of-scope reasoning). |
| 9 | `khive-runtime/src/atomic_prepare.rs:1331,1344` (`MergePlan` entity-merge edge rewire, ADR-099 atomic-merge draft) | N/A — `UPDATE graph_edges SET source_id = ?1, updated_at = ?2 WHERE source_id = ?3` / the `target_id` sibling. No `weight`/`metadata`/`target_backend`/natural-key conflict arm at all — this is a blind endpoint rewire (merge is deferred/unreachable through `--atomic`; see this crate's `prepare_merge` doc comment), not a conflict-arm statement. | Not applicable — different SQL shape, not a natural-key conflict site. |
| 10 | `khive-db/src/stores/graph.rs:168,179,188` (`edge_soft_delete_statement`, `edge_hard_delete_statement`, `purge_incident_edges_statement`) | N/A — plain delete/soft-delete DML, no conflict arm. | Not applicable. |
| 11 | `khive-runtime/src/atomic_prepare.rs:2097` (delete-plan soft-delete statement) | N/A — `deleted_at` tombstone only. | Not applicable. |
| 12 | `khive-runtime/src/atomic_runner.rs:535,560` and `khive-runtime/src/atomic_plan.rs:407` | N/A — `#[cfg(test)]`-only fixture SQL in unit tests exercising the runner mechanism generically (not edge-specific production behavior). | Not applicable (test-only). |
| 13 | `khive-db/src/stores/entity_tests.rs:717` | N/A — test fixture seeding a `graph_edges` row directly. | Not applicable (test-only). |

## Summary

- **Fixed:** site 3 (`batch_upsert_edges`) — previously a duplication site.
  No other production `INSERT`-shaped natural-key
  conflict arm remains outside `edge_upsert_statement`/
  `edge_insert_guarded_by_endpoints_statement`, both of which interpolate
  the single `EDGE_NATURAL_KEY_CONFLICT_SET` constant.
- **New gap surfaced by this audit, not fixed (out of approved scope):**
  sites 7 and 8 (`curation.rs`'s entity/note `merge` edge rewire) hand-copy
  the `EDGE_SYMMETRIC_CONFLICT_PROBE_SQL` /
  `EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL` /
  `EDGE_SYMMETRIC_REFRESH_CANONICAL_SQL` text a third and fourth time
  instead of referencing the constants directly. Recommended follow-up:
  have both call sites bind the existing `khive_db::stores::graph::
  EDGE_SYMMETRIC_*_SQL` constants (same pattern `operations.rs`'s
  `update_edge_symmetric_dml` already uses at site 5) instead of retyping
  the literal SQL strings.
- Every other `ON CONFLICT` / `graph_edges` DML site in the workspace is
  either the shared source of truth itself, a structurally distinct
  self-guarding atomic statement (justified-distinct, site 6), a
  non-conflict-arm statement (delete/soft-delete/rewire), or test-only
  fixture code.
