//! Session pack vocabulary: handler definitions and shared constants.

use khive_types::{HandlerDef, ParamDef, VerbCategory, Visibility};

pub(crate) const SESSION_KIND: &str = "session";
pub(crate) const DEFAULT_LIMIT: u32 = 20;
pub(crate) const MAX_LIMIT: u32 = 200;
pub(crate) const VALID_EXPORT_FORMATS: &[&str] = &["json", "markdown"];

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
            },
            ParamDef {
                name: "title",
                param_type: "string",
                required: false,
                description: "Human-readable session title stored as note.name.",
            },
            ParamDef {
                name: "provider",
                param_type: "string",
                required: false,
                description: "Provider label such as codex, claude_code, or openai.",
            },
            ParamDef {
                name: "provider_session_id",
                param_type: "string",
                required: false,
                description: "Provider-native continuity anchor.",
            },
            ParamDef {
                name: "tags",
                param_type: "array of string",
                required: false,
                description: "Caller labels stored in properties.tags.",
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
            },
            ParamDef {
                name: "offset",
                param_type: "integer",
                required: false,
                description: "Pagination offset; default 0.",
            },
            ParamDef {
                name: "provider",
                param_type: "string",
                required: false,
                description: "Exact filter on properties.provider.",
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
            },
            ParamDef {
                name: "format",
                param_type: "string",
                required: false,
                description: "json | markdown; default json.",
            },
        ],
    },
];
