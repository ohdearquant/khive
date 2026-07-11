-- V7: durable, non-reusing sequence for `notes` (khive #827).
--
-- `notes` has a TEXT PRIMARY KEY (`id`), so SQLite assigns it an *implicit*
-- rowid. Two things make an implicit rowid unsafe as a durable cursor key:
--   1. VACUUM may renumber implicit rowids (khive exposes `memory.vacuum`).
--   2. SQLite reuses the highest rowid after that row is deleted (khive
--      exposes a public hard delete of notes), so a later insert can land on
--      a rowid a caller's cursor has already passed, permanently excluding
--      that message from `comm.probe`.
--
-- `notes_seq` fixes both: its primary key is an explicit `INTEGER PRIMARY
-- KEY AUTOINCREMENT` column. An explicit integer primary key is never
-- renumbered by VACUUM (only implicit-rowid tables are), and AUTOINCREMENT
-- makes SQLite track the high-water mark in `sqlite_sequence`, so a value is
-- never reused even after its row is deleted.
--
-- Exactly one row is assigned per note `id`, the first time that id is
-- inserted (`INSERT OR IGNORE`), by the single writer inside the same
-- transaction as the note insert. The value then stays fixed for that note's
-- lifetime, including across an `INSERT OR REPLACE` delete+reinsert of the
-- same id (which churns `notes.rowid` but leaves `notes_seq` untouched).
CREATE TABLE IF NOT EXISTS notes_seq (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    note_id TEXT NOT NULL UNIQUE
);

CREATE INDEX IF NOT EXISTS idx_notes_seq_note_id ON notes_seq(note_id);
