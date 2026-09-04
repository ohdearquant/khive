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
-- A general `(to_actor, direction)` index for the comm mailbox listing was
-- also drafted for this change but dropped: with no ANALYZE statistics,
-- SQLite sometimes chose it over the existing partial
-- `idx_notes_unread_probe_recipient_direction` for the default unread-status
-- listing, even though the partial index (scoped by read status) is
-- strictly cheaper for that case -- confirmed by `EXPLAIN QUERY PLAN`
-- flipping to the new index and by VM-step regressions in
-- `crates/khive-db/src/stores/note_tests.rs`'s existing unread-probe tests.
-- Shipping it would have risked regressing the one path #2390 actually
-- measured (`comm.inbox` at unread-default status). The read/all-status
-- listing case it would have served remains unindexed.

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
