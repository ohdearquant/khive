-- Authority semantics and canonical payload formats belong to khive-capability.
-- UUIDs use canonical TEXT, timestamps use Unix microseconds, and opaque values
-- have separate bounds: canonical record bodies 2 MiB, other values 1 MiB.
-- The singleton constraint is independent of UUID identity.
CREATE TABLE IF NOT EXISTS authority_epochs (
    epoch_id TEXT PRIMARY KEY NOT NULL,
    singleton INTEGER NOT NULL CHECK (singleton = 1),
    marker_id TEXT NOT NULL UNIQUE,
    store_id TEXT NOT NULL,
    manifest_digest BLOB NOT NULL CHECK (typeof(manifest_digest) = 'blob' AND length(manifest_digest) = 32),
    manifest BLOB NOT NULL CHECK (typeof(manifest) = 'blob' AND length(manifest) BETWEEN 1 AND 1048576),
    namespace TEXT NOT NULL CHECK (length(CAST(namespace AS BLOB)) BETWEEN 1 AND 1048576),
    created_at INTEGER NOT NULL,
    CONSTRAINT authority_one_epoch UNIQUE (singleton)
);

CREATE TABLE IF NOT EXISTS authority_revisions (
    epoch_id TEXT PRIMARY KEY NOT NULL REFERENCES authority_epochs(epoch_id),
    revision INTEGER NOT NULL CHECK (typeof(revision) = 'integer' AND revision >= 0)
);

CREATE TABLE IF NOT EXISTS authority_records (
    record_id TEXT PRIMARY KEY NOT NULL,
    epoch_id TEXT NOT NULL REFERENCES authority_epochs(epoch_id),
    record_kind TEXT NOT NULL CHECK (record_kind IN ('grant', 'anchor')),
    principal BLOB NOT NULL CHECK (typeof(principal) = 'blob' AND length(principal) BETWEEN 1 AND 1048576),
    namespace TEXT NOT NULL CHECK (length(CAST(namespace AS BLOB)) BETWEEN 1 AND 1048576),
    parent_id TEXT,
    bootstrap_slot TEXT,
    root_anchor_id TEXT,
    created_at INTEGER NOT NULL,
    body BLOB NOT NULL CHECK (typeof(body) = 'blob' AND length(body) BETWEEN 1 AND 2097152),
    UNIQUE (epoch_id, record_id),
    FOREIGN KEY (epoch_id, parent_id) REFERENCES authority_records(epoch_id, record_id),
    FOREIGN KEY (epoch_id, root_anchor_id) REFERENCES authority_records(epoch_id, record_id),
    CHECK (parent_id IS NULL OR parent_id <> record_id),
    CHECK (root_anchor_id IS NULL OR root_anchor_id <> record_id),
    CHECK (
        (record_kind = 'anchor' AND parent_id IS NULL
            AND bootstrap_slot IS NULL AND root_anchor_id IS NULL)
        OR
        (record_kind = 'grant' AND root_anchor_id IS NOT NULL
            AND ((parent_id IS NOT NULL AND bootstrap_slot IS NULL)
                OR (parent_id IS NULL AND bootstrap_slot IS NOT NULL
                    AND length(CAST(bootstrap_slot AS BLOB)) BETWEEN 1 AND 1048576)))
    )
);
CREATE INDEX IF NOT EXISTS idx_authority_records_principal ON authority_records(epoch_id, principal, record_id);
CREATE INDEX IF NOT EXISTS idx_authority_records_scope ON authority_records(epoch_id, namespace, record_id);
CREATE INDEX IF NOT EXISTS idx_authority_records_parent ON authority_records(epoch_id, parent_id, record_id);
CREATE INDEX IF NOT EXISTS idx_authority_records_root ON authority_records(epoch_id, root_anchor_id, record_id);

-- Tags index all variants in a mixed grant. They are not permission payloads.
CREATE TABLE IF NOT EXISTS authority_record_right_types (
    epoch_id TEXT NOT NULL,
    record_id TEXT NOT NULL,
    right_type TEXT NOT NULL CHECK (length(CAST(right_type AS BLOB)) BETWEEN 1 AND 128),
    PRIMARY KEY (epoch_id, record_id, right_type),
    FOREIGN KEY (epoch_id, record_id) REFERENCES authority_records(epoch_id, record_id)
);
CREATE INDEX IF NOT EXISTS idx_authority_record_right_type ON authority_record_right_types(epoch_id, right_type, record_id);

-- Core events hold the one canonical Event envelope and payload. This table
-- provides the indexed action/affected-record relationship, not a second event.
CREATE TABLE IF NOT EXISTS authority_changes (
    event_id TEXT PRIMARY KEY NOT NULL REFERENCES events(id),
    epoch_id TEXT NOT NULL REFERENCES authority_epochs(epoch_id),
    action_id TEXT NOT NULL,
    record_kind TEXT NOT NULL CHECK (record_kind IN ('initialization', 'grant', 'anchor')),
    record_id TEXT NOT NULL,
    UNIQUE (epoch_id, event_id, action_id, record_id),
    UNIQUE (epoch_id, action_id, record_kind, record_id)
);
CREATE INDEX IF NOT EXISTS idx_authority_changes_action ON authority_changes(epoch_id, action_id, event_id);
CREATE INDEX IF NOT EXISTS idx_authority_changes_record ON authority_changes(epoch_id, record_id, event_id);

CREATE TABLE IF NOT EXISTS authority_transitions (
    transition_id TEXT PRIMARY KEY NOT NULL,
    epoch_id TEXT NOT NULL,
    record_id TEXT NOT NULL UNIQUE,
    terminal_state TEXT NOT NULL CHECK (terminal_state IN ('disabled', 'revoked', 'retired')),
    action_id TEXT NOT NULL,
    event_id TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    body BLOB NOT NULL CHECK (typeof(body) = 'blob' AND length(body) BETWEEN 1 AND 1048576),
    FOREIGN KEY (epoch_id, record_id) REFERENCES authority_records(epoch_id, record_id),
    FOREIGN KEY (epoch_id, event_id, action_id, record_id)
        REFERENCES authority_changes(epoch_id, event_id, action_id, record_id)
);

CREATE TABLE IF NOT EXISTS authority_replays (
    epoch_id TEXT NOT NULL REFERENCES authority_epochs(epoch_id),
    structural_caller BLOB NOT NULL CHECK (typeof(structural_caller) = 'blob' AND length(structural_caller) BETWEEN 1 AND 1048576),
    canonical_verb TEXT NOT NULL CHECK (length(CAST(canonical_verb AS BLOB)) BETWEEN 1 AND 1048576),
    request_id TEXT NOT NULL CHECK (length(CAST(request_id AS BLOB)) BETWEEN 1 AND 1048576),
    canonical_request BLOB NOT NULL CHECK (typeof(canonical_request) = 'blob' AND length(canonical_request) BETWEEN 1 AND 1048576),
    result BLOB NOT NULL CHECK (typeof(result) = 'blob' AND length(result) BETWEEN 1 AND 1048576),
    PRIMARY KEY (epoch_id, structural_caller, canonical_verb, request_id)
);
