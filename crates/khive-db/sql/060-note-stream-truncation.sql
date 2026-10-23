-- Persisted stream prefix floors and transaction-local delete permits (ADR-174 A3.2/A12).
CREATE TABLE IF NOT EXISTS note_stream_floors (
    namespace TEXT NOT NULL,
    stream TEXT NOT NULL,
    floor_seq INTEGER NOT NULL CHECK (floor_seq >= 1),
    PRIMARY KEY (namespace, stream)
);

CREATE TABLE IF NOT EXISTS note_stream_truncations (
    namespace TEXT NOT NULL,
    stream TEXT NOT NULL,
    before_seq INTEGER NOT NULL CHECK (before_seq >= 1),
    PRIMARY KEY (namespace, stream)
);

-- Versioned names keep a replay of an already-applied physical schema a no-op.
-- The other four V29 guards retain their exact historical definitions.
DROP TRIGGER IF EXISTS refuse_stream_gap;
CREATE TRIGGER IF NOT EXISTS refuse_stream_gap_v60
BEFORE INSERT ON note_streams
WHEN NEW.seq = 9223372036854775807
     OR NEW.seq != (
         SELECT MAX(
             COALESCE((SELECT MAX(seq) FROM note_streams
                       WHERE namespace = NEW.namespace AND stream = NEW.stream), 0),
             COALESCE((SELECT floor_seq FROM note_stream_floors
                       WHERE namespace = NEW.namespace AND stream = NEW.stream), 1) - 1
         ) + 1
     )
BEGIN
    SELECT RAISE(ABORT, 'stream_gap');
END;

DROP TRIGGER IF EXISTS refuse_stream_ledger_delete;
CREATE TRIGGER IF NOT EXISTS refuse_stream_ledger_delete_v60
BEFORE DELETE ON note_streams
WHEN NOT EXISTS (
    SELECT 1 FROM note_stream_truncations
    WHERE namespace = OLD.namespace AND stream = OLD.stream AND OLD.seq < before_seq
)
BEGIN
    SELECT RAISE(ABORT, 'stream_member');
END;
