-- V22: durable checkpoint cursor for the asynchronous brain fold worker.
--
-- One row per tailed event source. The first phase of the brain-state
-- decomposition (ADR-171) tails a single source — the domain store's own
-- `events` table, source = 'main' — and later phases add the audit-lane
-- store as a second row. The cursor advances by compare-and-set on
-- `last_rowid` INSIDE the same transaction as the fold it covers, so a
-- worker that loses the race aborts its whole unit and every feedback
-- event folds exactly once, however many processes run a worker.
CREATE TABLE IF NOT EXISTS brain_fold_cursor (
    source     TEXT PRIMARY KEY,
    last_rowid INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
