//! Session pack vocabulary: handler definitions.

use khive_types::{HandlerDef, ParamDef, VerbCategory, Visibility};

/// Handler table for the session pack.
///
/// All four verbs have `Visibility::Verb` (agent-facing MCP surface).
/// Speech-act categories follow ADR-025:
///   - `session.store` is a Directive (requests storage of content).
///   - `session.list`, `session.get`, `session.export` are Assertive (retrieve state).
pub(crate) static SESSION_HANDLERS: [HandlerDef; 4] = [
    HandlerDef {
        name: "session.store",
        description: "Store a session record (transcript, context snapshot, or accumulated agent state)",
        visibility: Visibility::Verb,
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
        visibility: Visibility::Verb,
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
        visibility: Visibility::Verb,
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
    HandlerDef {
        name: "session.export",
        description: "Serialize a session record for downstream use",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "id",
                param_type: "uuid",
                required: true,
                description: "Session UUID.",
            },
            ParamDef {
                name: "format",
                param_type: "string",
                required: false,
                description: "Export format: \"json\" (default) returns the full Note envelope; \"text\" returns content only.",
            },
        ],
    },
];
