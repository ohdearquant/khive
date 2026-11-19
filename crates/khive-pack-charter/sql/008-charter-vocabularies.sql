CREATE TABLE IF NOT EXISTS charter_vocabularies (
    policy_domain TEXT NOT NULL CHECK (length(policy_domain) > 0),
    name TEXT NOT NULL CHECK (length(name) > 0),
    version INTEGER NOT NULL CHECK (typeof(version) = 'integer' AND version > 0),
    namespace TEXT NOT NULL,
    source_digest TEXT NOT NULL CHECK (length(source_digest) > 0),
    source_bytes BLOB NOT NULL CHECK (typeof(source_bytes) = 'blob'),
    created_at_us INTEGER NOT NULL CHECK (created_at_us >= 0),
    PRIMARY KEY (policy_domain, name, version)
);

CREATE TRIGGER IF NOT EXISTS charter_vocabularies_no_replace
BEFORE INSERT ON charter_vocabularies
WHEN EXISTS (SELECT 1 FROM charter_vocabularies
    WHERE policy_domain = NEW.policy_domain AND name = NEW.name AND version = NEW.version)
BEGIN
    SELECT RAISE(ABORT, 'charter vocabularies are immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_vocabularies_no_update
BEFORE UPDATE ON charter_vocabularies
BEGIN
    SELECT RAISE(ABORT, 'charter vocabularies are immutable');
END;

CREATE TRIGGER IF NOT EXISTS charter_vocabularies_no_delete
BEFORE DELETE ON charter_vocabularies
BEGIN
    SELECT RAISE(ABORT, 'charter vocabularies are immutable');
END;
