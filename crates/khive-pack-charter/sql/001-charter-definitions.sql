CREATE TABLE IF NOT EXISTS charter_definitions (
    policy_domain TEXT NOT NULL CHECK (length(policy_domain) > 0),
    charter_id TEXT NOT NULL CHECK (length(charter_id) > 0),
    version INTEGER NOT NULL CHECK (typeof(version) = 'integer' AND version > 0),
    namespace TEXT NOT NULL,
    schema_version INTEGER NOT NULL CHECK (schema_version > 0),
    template_id TEXT NOT NULL CHECK (length(template_id) > 0),
    definition_digest TEXT NOT NULL CHECK (length(definition_digest) > 0),
    definition_bytes BLOB NOT NULL CHECK (typeof(definition_bytes) = 'blob'),
    gate_registry_digest TEXT NOT NULL CHECK (length(gate_registry_digest) > 0),
    action_contract_version TEXT NOT NULL CHECK (length(action_contract_version) > 0),
    created_at_us INTEGER NOT NULL CHECK (created_at_us >= 0),
    PRIMARY KEY (policy_domain, charter_id, version),
    UNIQUE (policy_domain, charter_id, version, definition_digest)
);

CREATE TRIGGER IF NOT EXISTS charter_definitions_no_replace
BEFORE INSERT ON charter_definitions
WHEN EXISTS (SELECT 1 FROM charter_definitions
    WHERE policy_domain = NEW.policy_domain AND charter_id = NEW.charter_id AND version = NEW.version)
BEGIN
    SELECT RAISE(ABORT, 'charter definitions are immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_definitions_no_update
BEFORE UPDATE ON charter_definitions
BEGIN
    SELECT RAISE(ABORT, 'charter definitions are immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_definitions_no_delete
BEFORE DELETE ON charter_definitions
BEGIN
    SELECT RAISE(ABORT, 'charter definitions are immutable');
END;
