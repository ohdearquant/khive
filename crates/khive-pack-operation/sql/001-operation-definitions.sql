CREATE TABLE IF NOT EXISTS operation_definitions (
    namespace TEXT NOT NULL CHECK (length(namespace) > 0),
    definition_id TEXT NOT NULL CHECK (length(definition_id) > 0),
    version INTEGER NOT NULL CHECK (typeof(version) = 'integer' AND version > 0),
    kind TEXT NOT NULL CHECK (kind IN ('operation_type', 'step_type')),
    digest TEXT NOT NULL CHECK (length(digest) > 0),
    definition_bytes BLOB NOT NULL CHECK (typeof(definition_bytes) = 'blob'),
    source_bytes TEXT NOT NULL CHECK (typeof(source_bytes) = 'text'),
    created_at_us INTEGER NOT NULL CHECK (created_at_us >= 0),
    PRIMARY KEY (namespace, definition_id, version)
);

CREATE TRIGGER IF NOT EXISTS operation_definitions_no_replace
BEFORE INSERT ON operation_definitions
WHEN EXISTS (SELECT 1 FROM operation_definitions
    WHERE namespace = NEW.namespace AND definition_id = NEW.definition_id AND version = NEW.version)
BEGIN
    SELECT RAISE(ABORT, 'operation definitions are immutable');
END;

CREATE TRIGGER IF NOT EXISTS operation_definitions_one_kind
BEFORE INSERT ON operation_definitions
WHEN EXISTS (SELECT 1 FROM operation_definitions
    WHERE namespace = NEW.namespace AND definition_id = NEW.definition_id AND kind != NEW.kind)
BEGIN
    SELECT RAISE(ABORT, 'an operation definition id keeps one kind');
END;

CREATE TRIGGER IF NOT EXISTS operation_definitions_no_update
BEFORE UPDATE ON operation_definitions
BEGIN
    SELECT RAISE(ABORT, 'operation definitions are immutable');
END;

CREATE TRIGGER IF NOT EXISTS operation_definitions_no_delete
BEFORE DELETE ON operation_definitions
BEGIN
    SELECT RAISE(ABORT, 'operation definitions are immutable');
END;
