# Probe cursor and channel checkpoints

Technical reference for `comm.probe`'s cursor semantics and the durable channel
poll checkpoint used by the polling loop (`comm.cursor_get`/`comm.cursor_commit`),
spanning `handlers.rs` and `vocab.rs`.

## `handlers.rs::PROBE_SQL`

The single indexed read powering `comm.probe` (ADR-D5). `INDEXED BY
idx_comm_message_to_actor` is a regression fence: if a custom bootstrap skips
comm schema-plan application, this query fails loudly instead of silently
degrading to a table scan.

`cursor_us`/`since_us` are keyed on `notes_seq.seq`, not SQLite `rowid` and
not `created_at` (#780, #827):

- `created_at` is an application-clock read taken before a note's write
  acquires the writer critical section, so two concurrent writers can commit
  out of stamp order; a `created_at`-keyed cursor can then advance past a row
  that committed *after* it, permanently hiding that row from every later
  probe.
- `notes.rowid` looked monotonic with commit order, but `notes` has a TEXT
  PRIMARY KEY, so that rowid is *implicit*: SQLite may renumber it on
  `VACUUM` (khive exposes `memory.vacuum`), and reuses the highest rowid once
  that row is hard-deleted (khive exposes a public hard delete), either of
  which can permanently exclude a later message whose rowid lands at or below
  an already-issued cursor.

`notes_seq.seq` fixes both: it is assigned once, inside the same writer
transaction as the note's insert, from a dedicated `INTEGER PRIMARY KEY
AUTOINCREMENT` sequence that VACUUM never renumbers and SQLite never reuses
(see `sql/007-notes-seq.sql`). The wire field names keep the `_us` suffix
(frozen contract, ADR-D5) but the value is an opaque monotonic token, not a
microsecond timestamp; do not revert this to `created_at` or `rowid`.
`created_at_us` on each `new_messages` entry is unaffected: it stays a real
display timestamp, still ordered ascending by `created_at` for readability,
and carries no cursor guarantee of its own.

## `handlers.rs::notes_seq_high_water_mark`

A caller-supplied `since_us` above `notes_seq`'s durable high-water mark
(`sqlite_sequence.seq` for the `notes_seq` table) cannot be a genuine cursor —
`notes_seq` starts at 1 and grows by exactly one per note ever inserted, so no
value this store ever handed out can exceed the highest value it has ever
assigned. Such a `since_us` is a pre-upgrade persisted-timestamp cursor
(#827): a real Unix-microsecond timestamp from after 1970-01-12 already
exceeds any realistic note count by orders of magnitude. Comparing against the
actual high-water mark, instead of a fixed ceiling, keeps this correct forever
as `notes_seq` grows — a fixed ceiling would eventually reset a legitimate
high sequence value to baseline, contradicting `comm.probe`'s opaque
round-trip contract.

`query_probe`'s cursor clamp: never let the returned cursor regress below what
the caller already holds (#827) — if the message that previously held the
highest `notes_seq.seq` was hard-deleted since the last probe, `MAX(seq)`
over the remaining rows can be smaller than a cursor already handed out.
Clamping in Rust, rather than in SQL, keeps the single indexed query a pure
aggregate with no extra branch.

`ProbeParams` is a public polling contract (khive #667 daemon hardening
slice) — its shape is frozen; see the comm pack README.

## `handlers.rs::handle_cursor_get` / `handle_cursor_commit`

Read/persist the durable channel poll checkpoint for `(channel_kind,
channel_slug)` (issue #449). Subhandlers — only the daemon's channel poll loop
calls these, and `cursor_commit` only after every envelope in the page has
returned `Ok` from `comm.ingest`. Both run the pack-owned
`comm_channel_cursor` schema statement before the query/write so an
in-memory/test runtime that never applied the boot-time schema plan still
works (matches the repository's lazy pack-schema bootstrap convention).
`cursor_get` returns JSON `null` when no row exists yet (first-run
compatibility mode). `cursor_commit` replaces any prior row for that identity.

## `vocab.rs::COMM_CHANNEL_CURSOR_SCHEMA_STMT`

Pack-owned auxiliary cursor table for durable channel poll progress (issue
#449): one row per `(channel_kind, channel_slug)`, holding the
transport-neutral checkpoint fields from `khive_channel::ChannelCheckpoint`.
For IMAP, `generation` is `UIDVALIDITY` and `high_water` is the greatest
durably handled UID. `source` detects a host/port/mailbox/folder change under
the same registry identity, so a stale checkpoint is never applied to a
different configuration.

Idempotent (`CREATE TABLE IF NOT EXISTS`), applied at boot via `schema_plan`
and shared verbatim with `handle_cursor_get`/`handle_cursor_commit`'s lazy
bootstrap for in-memory/test runtimes that never run the boot-time schema
plan.
