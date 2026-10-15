//! Session pack vocabulary: handler definitions and shared constants.

use khive_types::{
    HandlerDef, IdResolutionMode, PackColumnAddition, PackColumnAffinity, ParamDef, VerbCategory,
    Visibility,
};

pub(crate) const SESSION_KIND: &str = "session";
pub(crate) const DEFAULT_LIMIT: u32 = 20;
pub(crate) const MAX_LIMIT: u32 = 200;
pub(crate) const VALID_EXPORT_FORMATS: &[&str] = &["json", "markdown"];

pub(crate) static SESSION_SCHEMA_COLUMN_ADDITIONS: [PackColumnAddition; 1] = [PackColumnAddition {
    table: "session_mirror_cursor",
    column: "file_identity",
    affinity: PackColumnAffinity::Text,
}];

/// Pack-auxiliary schema for the session mirror tables.
///
/// Scoped mirror tables, identity constraints, and FTS5 index.
pub(crate) static SESSION_SCHEMA_PLAN_STMTS: [&str; 10] = [
    "CREATE TABLE IF NOT EXISTS sessions (\
        id                  TEXT NOT NULL,\
        provider_session_id TEXT NOT NULL,\
        source              TEXT NOT NULL DEFAULT 'claude_code',\
        cwd                 TEXT,\
        git_branch          TEXT,\
        slug                TEXT,\
        message_count       INTEGER NOT NULL DEFAULT 0,\
        first_seen_at       INTEGER NOT NULL,\
        last_seen_at        INTEGER NOT NULL,\
        namespace           TEXT NOT NULL,\
        UNIQUE (namespace, source, provider_session_id)\
    )",
    "CREATE INDEX IF NOT EXISTS idx_sessions_last_seen ON sessions(namespace, last_seen_at DESC)",
    "CREATE TABLE IF NOT EXISTS session_messages (\
        mirror_rowid    INTEGER PRIMARY KEY,\
        id              TEXT NOT NULL,\
        session_id      TEXT NOT NULL,\
        seq             INTEGER NOT NULL,\
        parent_uuid     TEXT,\
        is_sidechain    INTEGER NOT NULL DEFAULT 0,\
        role            TEXT,\
        msg_type        TEXT NOT NULL,\
        text            TEXT,\
        raw             TEXT NOT NULL,\
        created_at      INTEGER NOT NULL,\
        namespace       TEXT NOT NULL,\
        source          TEXT NOT NULL,\
        content_hash    TEXT NOT NULL,\
        UNIQUE (namespace, source, session_id, id)\
    )",
    "CREATE INDEX IF NOT EXISTS idx_session_messages_session \
      ON session_messages(namespace, source, session_id, seq)",
    "CREATE INDEX IF NOT EXISTS idx_session_messages_parent \
      ON session_messages(namespace, source, parent_uuid)",
    "CREATE VIRTUAL TABLE IF NOT EXISTS session_messages_fts \
      USING fts5(text, content='session_messages', content_rowid='mirror_rowid')",
    "CREATE TRIGGER IF NOT EXISTS session_messages_fts_ai AFTER INSERT ON session_messages BEGIN \
       INSERT INTO session_messages_fts(rowid, text) VALUES(new.mirror_rowid, new.text); END",
    "CREATE TRIGGER IF NOT EXISTS session_messages_fts_ad AFTER DELETE ON session_messages BEGIN \
       INSERT INTO session_messages_fts(session_messages_fts, rowid, text) \
       VALUES('delete', old.mirror_rowid, old.text); END",
    "CREATE TRIGGER IF NOT EXISTS session_messages_fts_au AFTER UPDATE OF text ON session_messages BEGIN \
       INSERT INTO session_messages_fts(session_messages_fts, rowid, text) \
       VALUES('delete', old.mirror_rowid, old.text); \
       INSERT INTO session_messages_fts(rowid, text) VALUES(new.mirror_rowid, new.text); END",
    "CREATE TABLE IF NOT EXISTS session_mirror_cursor (\
        file_path   TEXT PRIMARY KEY,\
        session_id  TEXT,\
        byte_offset INTEGER NOT NULL DEFAULT 0,\
        updated_at  INTEGER NOT NULL,\
        file_identity TEXT\
    )",
];

/// Speech-act categories follow ADR-025: `session.store` is a Directive
/// (requests storage of content); `session.list`, `session.resume`,
/// `session.export`, `session.search`, and `session.stats` are Assertive
/// (retrieve state); `session.vacuum` is Commissive (database maintenance).
pub(crate) static SESSION_HANDLERS: [HandlerDef; 7] = [
    HandlerDef {
        name: "session.store",
        description: "Persist an agent-session record as a session note",
        visibility: Visibility::Verb,
        category: VerbCategory::Directive,
        params: &[
            ParamDef {
                name: "content",
                param_type: "string",
                required: true,
                description: "Verbatim transcript or summary content.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "title",
                param_type: "string",
                required: false,
                description: "Human-readable session title stored as note.name.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "provider",
                param_type: "string",
                required: false,
                description: "Provider label such as codex, claude_code, or openai.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "provider_session_id",
                param_type: "string",
                required: false,
                description: "Provider-native continuity anchor.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "tags",
                param_type: "array of string",
                required: false,
                description: "Caller labels stored in properties.tags.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "session.list",
        description: "List stored sessions newest first",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Page size from 1 to 200; default 20.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "offset",
                param_type: "integer",
                required: false,
                description: "Pagination offset; default 0.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "provider",
                param_type: "string",
                required: false,
                description: "Exact filter on properties.provider.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "agent_id",
                param_type: "string",
                required: false,
                description: "Exact filter on properties.agent_id.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "since",
                param_type: "string",
                required: false,
                description: "RFC 3339 lower bound on note.created_at (inclusive).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "session.resume",
        description: "Fetch one session's full content by UUID or 8+ hex prefix",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[ParamDef {
            name: "id",
            param_type: "string",
            required: true,
            description: "Full UUID or 8+ hex short prefix.",
            resolution_mode: IdResolutionMode::NotApplicable,
        }],
    },
    HandlerDef {
        name: "session.export",
        description: "Serialize one stored session as json or markdown",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "id",
                param_type: "string",
                required: true,
                description: "Full UUID or 8+ hex short prefix.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "format",
                param_type: "string",
                required: false,
                description: "json | markdown; default json.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "session.search",
        description:
            "Search mirrored transcript text and return matching sessions within the caller's resolved tenant scope.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "query",
                param_type: "string",
                required: true,
                description: "Words to match in mirrored message text.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Maximum sessions from 1 to 200; default 20.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "since",
                param_type: "string",
                required: false,
                description: "Inclusive RFC 3339 lower bound on message creation time.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "source",
                param_type: "string",
                required: false,
                description:
                    "Exact mirror source; unknown selects migration orphans only when explicit.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "cwd",
                param_type: "string",
                required: false,
                description: "Exact session working-directory filter.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "session.stats",
        description: "Report database-wide session table rows and allocated bytes, plus database file and WAL sizes",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    },
    HandlerDef {
        name: "session.vacuum",
        description: "Reclaim unused SQLite pages and report database bytes before and after",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[],
    },
];

#[cfg(test)]
#[path = "vocab_tests.rs"]
mod tests;
