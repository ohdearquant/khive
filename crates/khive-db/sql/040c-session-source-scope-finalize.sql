-- Final mirror schema for fresh databases and retained legacy copies.
-- Rebuild FTS on all paths, including a pre-created final schema with rows.
CREATE TABLE IF NOT EXISTS sessions (
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

CREATE TABLE IF NOT EXISTS session_messages (
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

CREATE INDEX IF NOT EXISTS idx_sessions_last_seen
    ON sessions(namespace, last_seen_at DESC);
CREATE INDEX IF NOT EXISTS idx_session_messages_session
    ON session_messages(namespace, source, session_id, seq);
CREATE INDEX IF NOT EXISTS idx_session_messages_parent
    ON session_messages(namespace, source, parent_uuid);

CREATE VIRTUAL TABLE IF NOT EXISTS session_messages_fts
    USING fts5(text, content='session_messages', content_rowid='mirror_rowid');
CREATE TRIGGER IF NOT EXISTS session_messages_fts_ai
    AFTER INSERT ON session_messages BEGIN
    INSERT INTO session_messages_fts(rowid, text) VALUES(new.mirror_rowid, new.text);
END;
CREATE TRIGGER IF NOT EXISTS session_messages_fts_ad
    AFTER DELETE ON session_messages BEGIN
    INSERT INTO session_messages_fts(session_messages_fts, rowid, text)
        VALUES('delete', old.mirror_rowid, old.text);
END;
CREATE TRIGGER IF NOT EXISTS session_messages_fts_au
    AFTER UPDATE OF text ON session_messages BEGIN
    INSERT INTO session_messages_fts(session_messages_fts, rowid, text)
        VALUES('delete', old.mirror_rowid, old.text);
    INSERT INTO session_messages_fts(rowid, text) VALUES(new.mirror_rowid, new.text);
END;
INSERT INTO session_messages_fts(session_messages_fts) VALUES('rebuild');
