CREATE TABLE IF NOT EXISTS operations (
    operation_id TEXT NOT NULL PRIMARY KEY CHECK (length(operation_id) > 0),
    namespace TEXT NOT NULL CHECK (length(namespace) > 0),
    definition_namespace TEXT,
    definition_id TEXT,
    definition_version INTEGER,
    definition_digest TEXT,
    parent_operation_id TEXT,
    opened_by TEXT NOT NULL CHECK (length(opened_by) > 0),
    opened_at_us INTEGER NOT NULL CHECK (opened_at_us >= 0),
    CHECK ((definition_namespace IS NULL AND definition_id IS NULL
            AND definition_version IS NULL AND definition_digest IS NULL)
        OR (definition_namespace IS NOT NULL AND definition_id IS NOT NULL
            AND definition_version IS NOT NULL AND definition_digest IS NOT NULL)),
    CHECK (definition_namespace IS NULL OR definition_namespace != namespace),
    CHECK (parent_operation_id IS NULL OR parent_operation_id != operation_id),
    FOREIGN KEY (definition_namespace, definition_id, definition_version)
        REFERENCES operation_definitions (namespace, definition_id, version),
    FOREIGN KEY (parent_operation_id) REFERENCES operations (operation_id)
);

CREATE TRIGGER IF NOT EXISTS operations_no_replace
BEFORE INSERT ON operations
WHEN EXISTS (SELECT 1 FROM operations WHERE operation_id = NEW.operation_id)
BEGIN
    SELECT RAISE(ABORT, 'operations are append-only');
END;

CREATE TRIGGER IF NOT EXISTS operations_no_update
BEFORE UPDATE ON operations
BEGIN
    SELECT RAISE(ABORT, 'operations are append-only');
END;

CREATE TRIGGER IF NOT EXISTS operations_no_delete
BEFORE DELETE ON operations
BEGIN
    SELECT RAISE(ABORT, 'operations are append-only');
END;
