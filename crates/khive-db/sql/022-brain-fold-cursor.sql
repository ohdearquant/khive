-- V22: durable checkpoint cursor for the asynchronous brain fold worker.
--
-- One row per tailed event source. The first phase of the brain-state
-- decomposition (ADR-171) tails a single source — the domain store's own
-- `events` table, source = 'main' — and later phases add the audit-lane
-- store as a second row. The cursor advances by compare-and-set INSIDE the
-- same transaction as the fold it covers, so a worker that loses the race
-- aborts its whole unit and every feedback event folds exactly once,
-- however many processes run a worker.
--
-- The cursor is a (rowid, event id) witness pair, per ADR-171's replay
-- protocol. `events` is a rowid table with a TEXT primary key, so its rowid
-- is an authoritative insertion-order high-water mark that timestamps are
-- not (a delayed transaction lands with a later rowid even when its
-- `created_at` is older) — but an implicit rowid is renumbered by `VACUUM`,
-- so a bare rowid cursor could silently skip or re-fold rows after routine
-- maintenance. `last_event_id` witnesses that `last_rowid` still denotes
-- the same event: before trusting the seek position a worker validates the
-- witness; on mismatch it rebases by resolving the witness id's current
-- rowid and resumes past it; if the witnessed row is gone entirely
-- (tenure-purged) it resumes from the oldest remaining row — rewind, never
-- skip, because re-folding tolerant classes is recoverable noise while a
-- silent gap is unmeasurable loss.
CREATE TABLE IF NOT EXISTS brain_fold_cursor (
    source        TEXT PRIMARY KEY,
    last_rowid    INTEGER NOT NULL,
    last_event_id TEXT NOT NULL,
    updated_at    INTEGER NOT NULL
);
