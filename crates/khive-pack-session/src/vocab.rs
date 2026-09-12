//! Session pack vocabulary: handler definitions and shared constants.

use khive_types::{HandlerDef, IdResolutionMode, ParamDef, VerbCategory, Visibility};

pub(crate) const SESSION_KIND: &str = "session";
pub(crate) const DEFAULT_LIMIT: u32 = 20;
pub(crate) const MAX_LIMIT: u32 = 200;
pub(crate) const VALID_EXPORT_FORMATS: &[&str] = &["json", "markdown"];

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

/// Speech-act categories follow ADR-025: `session.store` is a Directive
/// (requests storage of content); `session.list`, `session.resume`, and
/// `session.export` are Assertive (retrieve state).
pub(crate) static SESSION_HANDLERS: [HandlerDef; 4] = [
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
];

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use khive_runtime::secret_gate::SESSION_MIRROR_STORED_TARGET;

    use super::SESSION_SCHEMA_PLAN_STMTS;

    /// Extract `(table_name, column_names)` from a `CREATE TABLE IF NOT
    /// EXISTS <name> (<col1 TYPE ..., col2 TYPE ...>)` statement. `None` for
    /// non-`CREATE TABLE` statements (the plan also carries `CREATE INDEX`).
    /// Column definitions here never contain a comma inside a quoted
    /// default, so splitting the body on `,` is exact for this schema.
    fn parse_ddl_columns(stmt: &str) -> Option<(String, Vec<String>)> {
        const MARKER: &str = "CREATE TABLE IF NOT EXISTS ";
        if !stmt.starts_with(MARKER) {
            return None;
        }
        let rest = &stmt[MARKER.len()..];
        let open = rest.find('(')?;
        let close = rest.rfind(')')?;
        let table = rest[..open].trim().to_string();
        let columns = rest[open + 1..close]
            .split(',')
            .filter_map(|col| col.split_whitespace().next().map(str::to_string))
            .collect();
        Some((table, columns))
    }

    /// Regression: `SESSION_MIRROR_STORED_TARGET` (`khive-runtime`'s
    /// executable ADR-115 contract) must name only columns that actually
    /// exist in the session mirror schema below — a stored-target string
    /// naming a nonexistent column silently overstates the masking
    /// guarantee. Fails before the fix on `session_messages.cwd` and
    /// `session_messages.git_branch`, neither of which is a real column
    /// (`cwd`/`git_branch` live only on `sessions`).
    #[test]
    fn session_mirror_stored_target_names_only_real_columns() {
        let mut tables: HashMap<String, Vec<String>> = HashMap::new();
        for stmt in &SESSION_SCHEMA_PLAN_STMTS {
            if let Some((table, columns)) = parse_ddl_columns(stmt) {
                tables.insert(table, columns);
            }
        }
        assert!(
            tables.contains_key("sessions") && tables.contains_key("session_messages"),
            "DDL parser must find both tables in the schema plan: {tables:?}"
        );

        for entry in SESSION_MIRROR_STORED_TARGET.split(',') {
            let entry = entry
                .trim()
                .trim_start_matches("and ")
                .trim_end_matches('.');
            let (table, column) = entry.split_once('.').unwrap_or_else(|| {
                panic!("stored-target entry {entry:?} is not table.column shaped")
            });
            let columns = tables.get(table).unwrap_or_else(|| {
                panic!("{table} named in SESSION_MIRROR_STORED_TARGET is not a real table")
            });
            assert!(
                columns.iter().any(|c| c == column),
                "{table}.{column} named in SESSION_MIRROR_STORED_TARGET does not exist; \
                 real {table} columns: {columns:?}"
            );
        }
    }
}
