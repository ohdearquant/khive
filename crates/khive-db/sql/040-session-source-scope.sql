-- ADR-117a Amendment 1: durable counts for the once-only mirror table rebuild.
-- The stage/swap/finalize SQL siblings are executed conditionally by the
-- migration runner in this same transaction. The Rust copy fills content hashes.
CREATE TABLE IF NOT EXISTS session_mirror_migration_audit (
    name          TEXT PRIMARY KEY,
    session_rows  INTEGER NOT NULL,
    message_rows  INTEGER NOT NULL,
    orphan_rows   INTEGER NOT NULL,
    cursors_reset INTEGER NOT NULL,
    migrated_at   INTEGER NOT NULL
);
