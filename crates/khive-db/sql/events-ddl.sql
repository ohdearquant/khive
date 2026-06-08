-- Events and event_observations tables and supporting indexes.
-- Applied idempotently by StorageBackend::events_for_namespace on every store access.

CREATE TABLE IF NOT EXISTS events (
    id                     TEXT PRIMARY KEY,
    namespace              TEXT NOT NULL,
    verb                   TEXT NOT NULL,
    substrate              TEXT NOT NULL,
    actor                  TEXT NOT NULL,
    kind                   TEXT NOT NULL DEFAULT 'audit',
    outcome                TEXT NOT NULL,
    payload                TEXT NOT NULL DEFAULT '{}',
    payload_schema_version INTEGER NOT NULL DEFAULT 1,
    profile_state_version  INTEGER,
    duration_us            INTEGER NOT NULL DEFAULT 0,
    target_id              TEXT,
    session_id             TEXT,
    aggregate_kind         TEXT,
    aggregate_id           TEXT,
    created_at             INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS event_observations (
    event_id      TEXT NOT NULL,
    entity_id     TEXT NOT NULL,
    referent_kind TEXT NOT NULL,
    role          TEXT NOT NULL,
    position      INTEGER NOT NULL,
    PRIMARY KEY (event_id, role, position)
);

CREATE INDEX IF NOT EXISTS idx_events_namespace ON events(namespace);
CREATE INDEX IF NOT EXISTS idx_events_verb ON events(verb);
CREATE INDEX IF NOT EXISTS idx_events_kind ON events(kind);
CREATE INDEX IF NOT EXISTS idx_events_substrate ON events(substrate);
CREATE INDEX IF NOT EXISTS idx_events_created ON events(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_events_ns_created_id ON events(namespace, created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_events_session ON events(namespace, session_id, created_at, id);
CREATE INDEX IF NOT EXISTS idx_events_payload_proposal_id ON events(json_extract(payload, '$.proposal_id'));
CREATE INDEX IF NOT EXISTS idx_event_obs_entity ON event_observations(entity_id, role);
CREATE INDEX IF NOT EXISTS idx_event_obs_event_role ON event_observations(event_id, role);
