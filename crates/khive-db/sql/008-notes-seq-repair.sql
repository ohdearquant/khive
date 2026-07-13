-- V8: repair partially-populated `notes_seq` ledgers (khive #827).
--
-- V7 (`007-notes-seq.sql`) backfilled `notes_seq` with an unconditional
-- `INSERT OR IGNORE ... SELECT`, then recorded itself as applied in
-- `_schema_migrations` -- so V7's body never runs again on that database.
-- A database that ran V7, then received exactly one post-upgrade note
-- (correctly assigned its own `notes_seq` row via `assign_note_seq`), ends
-- up with a *partially populated* `notes_seq`: one note mapped, every
-- pre-V7 note still missing a row, and no migration left that will ever
-- revisit them. `comm.probe`'s `INNER JOIN notes_seq` silently drops every
-- one of those older notes forever.
--
-- This migration repairs any note still missing a `notes_seq` row,
-- regardless of how many rows already exist -- the anti-join targets
-- missing ids specifically, not "notes_seq is empty" -- so it is correct
-- to layer on top of any prior state: a fresh V7 backfill, a partial
-- backfill, or an already fully populated ledger. `INSERT OR IGNORE` makes
-- it idempotent and safe to run more than once.
INSERT OR IGNORE INTO notes_seq (note_id)
SELECT n.id FROM notes n
WHERE NOT EXISTS (SELECT 1 FROM notes_seq s WHERE s.note_id = n.id)
ORDER BY n.created_at ASC, n.id ASC;
