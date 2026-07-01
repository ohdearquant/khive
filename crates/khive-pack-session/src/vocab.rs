//! Session pack vocabulary: handler definitions.

use khive_types::{HandlerDef, ParamDef, VerbCategory, Visibility};

/// Pack-auxiliary schema for the session mirror tables.
///
/// Three tables + three indexes, all idempotent (`CREATE TABLE/INDEX IF NOT EXISTS`).
/// Applied at boot via the `schema_plan` hook and lazily in tests via `execute_script`.
pub(crate) static SESSION_SCHEMA_PLAN_STMTS: [&str; 6] = [
    "CREATE TABLE IF NOT EXISTS sessions (\
        id                  TEXT PRIMARY KEY,\
        provider_session_id TEXT NOT NULL,\
        source              TEXT NOT NULL DEFAULT 'claude_code',\
        cwd                 TEXT,\
        git_branch          TEXT,\
        slug                TEXT,\
        message_count       INTEGER NOT NULL DEFAULT 0,\
        first_seen_at       INTEGER NOT NULL,\
        last_seen_at        INTEGER NOT NULL,\
        namespace           TEXT\
    )",
    "CREATE INDEX IF NOT EXISTS idx_sessions_last_seen ON sessions(last_seen_at DESC)",
    "CREATE TABLE IF NOT EXISTS session_messages (\
        id              TEXT PRIMARY KEY,\
        session_id      TEXT NOT NULL,\
        seq             INTEGER NOT NULL,\
        parent_uuid     TEXT,\
        is_sidechain    INTEGER NOT NULL DEFAULT 0,\
        role            TEXT,\
        msg_type        TEXT NOT NULL,\
        text            TEXT,\
        raw             TEXT NOT NULL,\
        created_at      INTEGER NOT NULL,\
        namespace       TEXT\
    )",
    "CREATE INDEX IF NOT EXISTS idx_session_messages_session ON session_messages(session_id, seq)",
    "CREATE INDEX IF NOT EXISTS idx_session_messages_parent  ON session_messages(parent_uuid)",
    "CREATE TABLE IF NOT EXISTS session_mirror_cursor (\
        file_path   TEXT PRIMARY KEY,\
        session_id  TEXT,\
        byte_offset INTEGER NOT NULL DEFAULT 0,\
        updated_at  INTEGER NOT NULL\
    )",
];

/// Handler table for the session pack.
///
/// All three verbs are `Visibility::Subhandler` (operator-only, NOT on the
/// agent-facing MCP `request` surface) for this milestone. The session pack's
/// only active feature is the background daemon mirror (the `warm()` hook),
/// which runs independent of verb visibility. The read/query layer
/// (store/list/get) is deferred — it stays dispatchable via the runtime and
/// `kkernel exec` but is withheld from the agent surface until the
/// session-continuity query UX is designed. Flip to `Visibility::Verb` to
/// expose (and bump the smoke-test verb count accordingly).
///
/// Serialization is NOT a verb: `handle_export` is an internal helper called
/// in-process, not dispatched through the DSL, so it has no `HandlerDef` entry.
///
/// Speech-act categories follow ADR-025:
///   - `session.store` is a Directive (requests storage of content).
///   - `session.list`, `session.get` are Assertive (retrieve state).
pub(crate) static SESSION_HANDLERS: [HandlerDef; 3] = [
    HandlerDef {
        name: "session.store",
        description: "Store a session record (transcript, context snapshot, or accumulated agent state)",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Directive,
        params: &[
            ParamDef {
                name: "content",
                param_type: "string",
                required: true,
                description: "Session content: transcript, context snapshot, or arbitrary text.",
            },
            ParamDef {
                name: "agent_id",
                param_type: "string",
                required: false,
                description: "Agent identifier stored in properties.agent_id; used as a list filter.",
            },
            ParamDef {
                name: "tags",
                param_type: "array of string",
                required: false,
                description: "Tag list stored in properties.tags.",
            },
            ParamDef {
                name: "metadata",
                param_type: "object",
                required: false,
                description: "Arbitrary JSON object merged into properties. Explicit agent_id and tags params take precedence.",
            },
        ],
    },
    HandlerDef {
        name: "session.list",
        description: "List stored sessions, newest first",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "agent_id",
                param_type: "string",
                required: false,
                description: "Filter by properties.agent_id.",
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Page size (default 20, max 200).",
            },
            ParamDef {
                name: "offset",
                param_type: "integer",
                required: false,
                description: "Pagination offset (default 0).",
            },
            ParamDef {
                name: "since",
                param_type: "string",
                required: false,
                description: "ISO-8601 datetime; returns only sessions with created_at >= since.",
            },
        ],
    },
    HandlerDef {
        name: "session.get",
        description: "Fetch a single session record by UUID for replay or context injection",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "id",
                param_type: "uuid",
                required: true,
                description: "Session UUID.",
            },
        ],
    },
];
