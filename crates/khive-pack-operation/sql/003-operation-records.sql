CREATE TABLE IF NOT EXISTS operation_records (
    operation_id TEXT NOT NULL,
    sequence INTEGER NOT NULL CHECK (typeof(sequence) = 'integer' AND sequence > 0),
    namespace TEXT NOT NULL CHECK (length(namespace) > 0),
    kind TEXT NOT NULL CHECK (kind IN
        ('binding', 'document', 'fact', 'act', 'observation', 'variant', 'typing')),
    caller_key TEXT NOT NULL CHECK (length(caller_key) > 0),
    payload_digest TEXT NOT NULL CHECK (length(payload_digest) > 0),
    payload_bytes BLOB NOT NULL CHECK (typeof(payload_bytes) = 'blob'),
    observed_at_us INTEGER NOT NULL CHECK (observed_at_us >= 0),
    recorded_at_us INTEGER NOT NULL CHECK (recorded_at_us >= 0),
    supersedes_sequence INTEGER CHECK (supersedes_sequence > 0 AND supersedes_sequence < sequence),
    PRIMARY KEY (operation_id, sequence),
    UNIQUE (operation_id, caller_key),
    FOREIGN KEY (operation_id) REFERENCES operations (operation_id),
    FOREIGN KEY (operation_id, supersedes_sequence)
        REFERENCES operation_records (operation_id, sequence)
);

CREATE TRIGGER IF NOT EXISTS operation_records_no_replace
BEFORE INSERT ON operation_records
WHEN EXISTS (SELECT 1 FROM operation_records
    WHERE operation_id = NEW.operation_id
        AND (sequence = NEW.sequence OR caller_key = NEW.caller_key))
BEGIN
    SELECT RAISE(ABORT, 'operation records are immutable');
END;

CREATE TRIGGER IF NOT EXISTS operation_records_dense_sequence
BEFORE INSERT ON operation_records
WHEN NEW.sequence != COALESCE((SELECT MAX(sequence) FROM operation_records
    WHERE operation_id = NEW.operation_id), 0) + 1
BEGIN
    SELECT RAISE(ABORT, 'operation record sequence must be dense');
END;

CREATE TRIGGER IF NOT EXISTS operation_records_operation_namespace
BEFORE INSERT ON operation_records
WHEN NOT EXISTS (SELECT 1 FROM operations
    WHERE operation_id = NEW.operation_id AND namespace = NEW.namespace)
BEGIN
    SELECT RAISE(ABORT, 'an operation record lives in its operation''s namespace');
END;

CREATE TRIGGER IF NOT EXISTS operation_records_no_update
BEFORE UPDATE ON operation_records
BEGIN
    SELECT RAISE(ABORT, 'operation records are immutable');
END;

CREATE TRIGGER IF NOT EXISTS operation_records_no_delete
BEFORE DELETE ON operation_records
BEGIN
    SELECT RAISE(ABORT, 'operation records are immutable');
END;
