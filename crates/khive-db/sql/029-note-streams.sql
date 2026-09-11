-- Ordered, namespace-scoped immutable note streams (ADR-174, Amendment 2).
CREATE TABLE IF NOT EXISTS note_streams (
    namespace TEXT NOT NULL,
    stream TEXT NOT NULL,
    seq INTEGER NOT NULL CHECK (seq > 0),
    note_id TEXT NOT NULL UNIQUE,
    PRIMARY KEY (namespace, stream, seq),
    FOREIGN KEY (note_id) REFERENCES notes (id)
);

CREATE TRIGGER IF NOT EXISTS refuse_stream_foreign_note
BEFORE INSERT ON note_streams
WHEN NOT EXISTS (SELECT 1 FROM notes
                 WHERE id = NEW.note_id AND namespace = NEW.namespace AND deleted_at IS NULL)
     OR EXISTS (SELECT 1 FROM note_streams WHERE note_id = NEW.note_id)
BEGIN
    SELECT RAISE(ABORT, 'stream_member');
END;

CREATE TRIGGER IF NOT EXISTS refuse_stream_gap
BEFORE INSERT ON note_streams
WHEN NEW.seq != (SELECT COALESCE(MAX(seq), 0) + 1 FROM note_streams
                 WHERE namespace = NEW.namespace AND stream = NEW.stream)
BEGIN
    SELECT RAISE(ABORT, 'stream_gap');
END;

CREATE TRIGGER IF NOT EXISTS refuse_stream_ledger_delete
BEFORE DELETE ON note_streams
BEGIN
    SELECT RAISE(ABORT, 'stream_member');
END;

CREATE TRIGGER IF NOT EXISTS refuse_stream_ledger_update
BEFORE UPDATE ON note_streams
BEGIN
    SELECT RAISE(ABORT, 'stream_member');
END;

CREATE TRIGGER IF NOT EXISTS refuse_stream_entry_delete
BEFORE DELETE ON notes
WHEN EXISTS (SELECT 1 FROM note_streams WHERE note_id = OLD.id)
BEGIN
    SELECT RAISE(ABORT, 'stream_member');
END;

CREATE TRIGGER IF NOT EXISTS refuse_stream_entry_rewrite
BEFORE UPDATE OF content, properties, deleted_at, namespace, kind ON notes
WHEN EXISTS (SELECT 1 FROM note_streams WHERE note_id = OLD.id)
BEGIN
    SELECT RAISE(ABORT, 'stream_member');
END;
