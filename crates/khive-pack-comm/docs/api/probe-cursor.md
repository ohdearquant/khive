# Probe cursor and channel checkpoints

Technical reference for `comm.probe`'s cursor semantics and the durable channel
poll checkpoint used by the polling loop (`comm.cursor_get`/`comm.cursor_commit`),
spanning `handlers.rs` and `vocab.rs`.

## `handlers.rs::PROBE_SQL`

The page and stale unread count are computed by one SQL statement in the same
snapshot. Both are scoped to live inbound `message` notes in the caller's
namespace whose `to_actor` exactly equals the requested actor. Missing or null
recipients do not enter this probe through the inbox's legacy visibility rules.

Page selection takes the first 100 matching rows with `notes_seq.seq` above
the honored caller cursor, ordered by sequence ascending. The returned cursor
is the maximum sequence among those returned rows, floored by the honored
caller cursor. An empty page does not advance the cursor; an empty baseline
returns zero. This lets callers drain bursts larger than 100 messages without
advancing past rows they have not received (#2595). The selected page is then
ordered by `created_at` ascending for display.

`stale_unread_count` is `min(exact stale unread count, 1000)`: values below
1000 are exact, and 1000 means at least 1000. It is independent of the arrival
cursor and page limit. Staleness uses the strict predicate `created_at < cutoff`,
where the default cutoff is 20 minutes before the probe. A note is unread unless
`json_type(properties, '$.read')` is `true`; absent, null, and non-boolean values
remain unread. The count uses the existing partial
`idx_notes_unread_probe_recipient_direction` index and limits qualifying rows
before aggregation. This bounds the stale count's work without claiming a bound
on all page-selection work.

The storage helper `count_notes_filtered_bounded_in_snapshot` is not used here:
its filter has no before-cutoff predicate, and a separate count invocation would
not share this statement's snapshot with the returned page. `comm.inbox`'s
unread-count partitions and contract are unchanged.

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

An above-high-water cursor is discarded before page selection, and the response
reports `cursor_reset: true` (#2400). The marker is absent when no cursor was
supplied or the supplied cursor was honored. The global durable high-water mark
validates a supplied cursor; it does not advance the page cursor.

`query_probe` retains the caller-cursor floor (#827): deleting a previously
returned message must not make a later cursor regress. The floor uses the
honored cursor after any reset. It also preserves the cursor when the next
page is empty, independently of the maximum sequence in the remaining corpus.

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
