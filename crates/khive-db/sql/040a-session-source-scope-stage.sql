-- Temporary target tables for the ADR-117a legacy mirror rebuild.
-- The global runner executes this only when it has identified the exact
-- legacy bare-id schema. SQL lint replays the complete group on a fresh DB.
CREATE TABLE sessions_scope_new (
    id                  TEXT NOT NULL,
    provider_session_id TEXT NOT NULL,
    source              TEXT NOT NULL DEFAULT 'claude_code',
    cwd                 TEXT,
    git_branch          TEXT,
    slug                TEXT,
    message_count       INTEGER NOT NULL DEFAULT 0,
    first_seen_at       INTEGER NOT NULL,
    last_seen_at        INTEGER NOT NULL,
    namespace           TEXT NOT NULL,
    UNIQUE (namespace, source, provider_session_id)
);

CREATE TABLE session_messages_scope_new (
    mirror_rowid INTEGER PRIMARY KEY,
    id           TEXT NOT NULL,
    session_id   TEXT NOT NULL,
    seq          INTEGER NOT NULL,
    parent_uuid  TEXT,
    is_sidechain INTEGER NOT NULL DEFAULT 0,
    role         TEXT,
    msg_type     TEXT NOT NULL,
    text         TEXT,
    raw          TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    namespace    TEXT NOT NULL,
    source       TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    UNIQUE (namespace, source, session_id, id)
);
