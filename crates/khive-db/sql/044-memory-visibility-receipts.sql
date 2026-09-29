-- ADR-144 Amendment 1: a keyed memory's original vector-write fences survive
-- exact replay even after the ANN delta log is compacted. The header records
-- a successful zero-model write distinctly from a pre-migration holder with
-- no receipt. Both tables are populated inside the keyed memory's atomic unit.
CREATE TABLE IF NOT EXISTS memory_visibility_receipts (
    namespace TEXT NOT NULL,
    note_id TEXT NOT NULL,
    model_count INTEGER NOT NULL CHECK (model_count >= 0),
    PRIMARY KEY (namespace, note_id),
    FOREIGN KEY (note_id) REFERENCES notes(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS memory_visibility_receipts_note_id
    ON memory_visibility_receipts (note_id);

CREATE TABLE IF NOT EXISTS memory_visibility_fences (
    namespace TEXT NOT NULL,
    note_id TEXT NOT NULL,
    model TEXT NOT NULL CHECK (length(model) > 0),
    ann_write_log_seq INTEGER NOT NULL CHECK (ann_write_log_seq > 0),
    PRIMARY KEY (namespace, note_id, model),
    FOREIGN KEY (namespace, note_id)
        REFERENCES memory_visibility_receipts(namespace, note_id) ON DELETE CASCADE
);
