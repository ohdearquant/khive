-- V27: index the hot note property paths behind GTD task listing
-- (gtd.tasks / gtd.next).
--
-- The GTD pack pushes its status/assignee predicates into SQL
-- (`json_extract(properties, '$.status')` / `'$.assignee'`), but with no
-- supporting index every task listing evaluates those expressions against
-- every row of kind='task' in the namespace: the planner can only narrow to
-- `idx_notes_kind`'s (namespace, kind) partition, then walks and JSON-parses
-- every task row regardless of how selective the status/assignee filter is.
--
-- Each expression below is copied verbatim from the SQL the corresponding
-- `FilterOp` compiles (see `build_note_filter_where` in
-- `crates/khive-db/src/stores/note.rs`) -- an index only helps when its key
-- expression matches the compiled predicate byte-for-byte.
--
-- Both indexes are partial on `deleted_at IS NULL` only, not additionally on
-- `kind = 'task'`: every caller binds `kind` as a parameter rather than a
-- literal, and SQLite can use a partial index only when it can prove the
-- index's WHERE clause at plan time, which a bound parameter never
-- satisfies (`crates/khive-db/src/stores/note_tests.rs`'s
-- `narrowing_hot_property_index_to_kind_task_is_not_chosen` measures this).
--
-- These indexes cover only the GTD task-listing path. Two related paths
-- remain unindexed. The comm inbox listing's `status="all"`/`status="read"`
-- case (no `$.read` predicate, or `$.read = true` rather than the tuned
-- `$.read` absent-or-false probe) and the default `gtd.tasks()` listing (no
-- `status=` filter, which compiles to an open `$.status NOT IN
-- ('done','cancelled') OR $.status IS NULL` exclusion with no bounded set to
-- seek) both still cost a full partition scan. A general
-- `(namespace, kind, created_at DESC, id ASC)` index would serve the first
-- of those, but it is also a legal plan for `comm.inbox`'s default
-- `status="unread"` listing, and with no ANALYZE statistics SQLite prefers
-- it there over the purpose-built partial
-- `idx_notes_unread_probe_recipient_direction`, even though the partial
-- index is cheaper for that case -- confirmed by `EXPLAIN QUERY PLAN`
-- flipping to the general index and by VM-step regressions in
-- `crates/khive-db/src/stores/note_tests.rs`'s
-- `candidate_created_at_id_seek_index_flips_unread_probe_plan`. A narrower
-- `(to_actor, direction)` variant carries the identical conflict. The
-- `gtd.tasks()` default listing has no seekable rewrite at all: the
-- exclusion set must stay open (a task with a missing or unrecognized
-- status is included), so it cannot be rewritten as a bounded `IN` list
-- without silently dropping exactly the rows this predicate exists to keep.

CREATE INDEX IF NOT EXISTS idx_notes_task_status
    ON notes(namespace, kind,
             json_extract(properties, '$.status'),
             created_at DESC, id ASC)
    WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_notes_task_assignee
    ON notes(namespace, kind,
             json_extract(properties, '$.assignee'),
             created_at DESC, id ASC)
    WHERE deleted_at IS NULL;
