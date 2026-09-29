# Session store diagnostics and compaction

`session.stats()` reports the database-wide row count and allocated bytes for
`sessions`, `session_messages`, `session_mirror_cursor`, and
`session_messages_fts`. The byte figures use SQLite `dbstat` and include each
table's indexes; the FTS figure includes its shadow tables. The response also
reports `page_size_bytes`, `page_count`, allocated `database_bytes`, and the
current database-file and WAL sizes. File and WAL sizes are `null` for an
in-memory backend; an absent WAL sidecar reports zero bytes. If SQLite lacks
`dbstat`, the verb fails explicitly rather than inventing per-table byte
figures. These are on-demand observations, not an atomic snapshot:
concurrent writes can change them while the verb runs. All figures are for the
selected database, not only the caller's namespace, as the response's scope
fields state.

`session.vacuum()` explicitly runs SQLite `VACUUM` through the shared top-level
writer, then returns allocated database bytes, page counts, and database-file
and WAL sizes before and after the operation. `allocated_bytes_reclaimed` is
the decrease in `page_count × page_size`; a WAL-backed database's physical
file size can differ until a checkpoint. This operation can be costly and can
wait for active readers. It does not delete session rows or set a retention
policy. The mirror currently exposes no pass-in-progress signal, so the verb
serializes with mirror database writes through the writer but cannot refuse an
entire mirror pass in progress.

Once VACUUM commits, the response retains `ok: true` even if a request read
deadline or another post-commit measurement error prevents the after figures.
In that case `post_vacuum_metrics_status` is `unavailable_after_commit`,
`post_vacuum_metrics_error` names the measurement error, and the after and
reclaimed fields are `null`. A successful measurement reports status
`available`. SQLite's VACUUM may use an in-memory temporary copy because this
backend configures `temp_store=MEMORY`; operators should budget memory for a
large database.
