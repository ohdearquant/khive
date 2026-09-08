//! Vocabulary, schema plan and handler table for the tool pack.

use khive_types::{EntityKind, EntityTypeDef, HandlerDef, IdResolutionMode, ParamDef, VerbCategory, Visibility};

/// Canonical pack name; every verb is `tool.<name>`.
pub const PACK_NAME: &str = "tool";

/// Tag carried by every registry object so listing and search can select
/// the registry without a schema change.
pub const REGISTRY_TAG: &str = "tool-registry";

/// Tag carried by every capability concept.
pub const CAPABILITY_TAG: &str = "tool-capability";

/// Base entity kind of a registry object.
pub const REGISTRY_ENTITY_KIND: &str = "project";

/// Registry object subtypes (`entity_type` on the `project` kind).
pub const KINDS: &[&str] = &["tool", "skill", "plugin", "verb"];

/// Side-effect classes, from harmless to unrecoverable. The default policy
/// allows `read` and asks for everything else.
pub const SIDE_EFFECTS: &[&str] = &["read", "write", "egress", "irreversible"];

/// Trust origins.
pub const TRUST_ORIGINS: &[&str] = &["first_party", "marketplace", "external"];

/// Policy decisions.
pub const DECISIONS: &[&str] = &["allow", "deny", "ask"];

/// Grant statuses.
pub const GRANT_STATUSES: &[&str] = &["requested", "granted", "denied", "revoked"];

/// Entity subtypes this pack adds on top of the builtin registry. `tool`
/// already exists on the `project` kind; the other three registry kinds and
/// the capability concept subtype are new.
pub static ENTITY_TYPES: [EntityTypeDef; 4] = [
    EntityTypeDef {
        kind: EntityKind::Project,
        type_name: "skill",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Project,
        type_name: "plugin",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Project,
        type_name: "verb",
        aliases: &[],
    },
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "capability",
        aliases: &[],
    },
];

/// Pack-owned tables, applied idempotently at boot.
pub static TOOL_SCHEMA_PLAN_STMTS: [&str; 4] = [
    "CREATE TABLE IF NOT EXISTS tool_policy (\
        id         TEXT PRIMARY KEY,\
        namespace  TEXT NOT NULL,\
        actor      TEXT NOT NULL,\
        tool       TEXT NOT NULL,\
        decision   TEXT NOT NULL,\
        note       TEXT,\
        created_at INTEGER NOT NULL,\
        created_by TEXT\
    )",
    "CREATE INDEX IF NOT EXISTS idx_tool_policy_lookup ON tool_policy(namespace, actor, tool)",
    "CREATE TABLE IF NOT EXISTS tool_grants (\
        id            TEXT PRIMARY KEY,\
        namespace     TEXT NOT NULL,\
        actor         TEXT NOT NULL,\
        tool          TEXT NOT NULL,\
        scope         TEXT,\
        reason        TEXT,\
        status        TEXT NOT NULL,\
        requested_at  INTEGER NOT NULL,\
        decided_at    INTEGER,\
        decided_by    TEXT,\
        expires_at    INTEGER,\
        decision_note TEXT\
    )",
    "CREATE INDEX IF NOT EXISTS idx_tool_grants_lookup ON tool_grants(namespace, actor, tool, status)",
];

const P_NAMESPACE: ParamDef = ParamDef {
    name: "namespace",
    param_type: "string",
    required: false,
    description: "Namespace to operate in (defaults to the caller's).",
    resolution_mode: IdResolutionMode::NotApplicable,
};

const P_ACTOR: ParamDef = ParamDef {
    name: "actor",
    param_type: "string",
    required: false,
    description: "Actor label the decision is evaluated for (defaults to the caller's actor).",
    resolution_mode: IdResolutionMode::NotApplicable,
};

const P_TOOL: ParamDef = ParamDef {
    name: "tool",
    param_type: "string",
    required: true,
    description: "Registry object name (for example `comm.send` or an MCP tool name) or its id.",
    resolution_mode: IdResolutionMode::NotApplicable,
};

pub static TOOL_HANDLERS: [HandlerDef; 13] = [
    HandlerDef {
        name: "tool.register",
        description: "Register one tool, skill, plugin or verb in the registry, with its capabilities, side-effect class and trust origin. Idempotent by name.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef { name: "name", param_type: "string", required: true, description: "Unique registry name.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "kind", param_type: "string", required: false, description: "tool (default), skill, plugin or verb.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "description", param_type: "string", required: false, description: "What it does; indexed for discovery.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "schema", param_type: "object", required: false, description: "Callable contract (for example an MCP inputSchema).", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "source", param_type: "string", required: false, description: "Where it is installed, for example mcp:<server>, skills:<dir>, khive:<pack>.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "side_effect", param_type: "string", required: false, description: "read, write, egress or irreversible (default write).", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "trust", param_type: "string", required: false, description: "first_party, marketplace or external (default external).", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "capabilities", param_type: "array of string", required: false, description: "Capability concept names this object implements; created when absent.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "tags", param_type: "array of string", required: false, description: "Extra tags.", resolution_mode: IdResolutionMode::NotApplicable },
            P_NAMESPACE,
        ],
    },
    HandlerDef {
        name: "tool.ingest",
        description: "Bulk-register from a source: khive (every loaded verb) or mcp (a tools/list payload).",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef { name: "source", param_type: "string", required: true, description: "khive or mcp.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "server", param_type: "string", required: false, description: "MCP server name (source=mcp).", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "tools", param_type: "array of object", required: false, description: "MCP tools/list entries: name, description, inputSchema, optional capabilities and side_effect (source=mcp).", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "trust", param_type: "string", required: false, description: "Trust origin applied to every ingested object.", resolution_mode: IdResolutionMode::NotApplicable },
            P_NAMESPACE,
        ],
    },
    HandlerDef {
        name: "tool.suggest",
        description: "Discover registry objects for a need: text and vector search over the registry plus capability concepts expanded along implements edges; every hit carries the caller's policy decision.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef { name: "query", param_type: "string", required: true, description: "What the caller needs to do.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "limit", param_type: "integer", required: false, description: "Maximum hits (default 10).", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "kind", param_type: "string", required: false, description: "Restrict to tool, skill, plugin or verb.", resolution_mode: IdResolutionMode::NotApplicable },
            P_ACTOR,
            P_NAMESPACE,
        ],
    },
    HandlerDef {
        name: "tool.describe",
        description: "Full record of one registry object: contract, source, side-effect class, trust, capabilities, and the caller's decision.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[P_TOOL, P_ACTOR, P_NAMESPACE],
    },
    HandlerDef {
        name: "tool.list",
        description: "List registry objects, optionally by kind.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef { name: "kind", param_type: "string", required: false, description: "tool, skill, plugin or verb.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "limit", param_type: "integer", required: false, description: "Page size (default 100).", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "offset", param_type: "integer", required: false, description: "Page offset.", resolution_mode: IdResolutionMode::NotApplicable },
            P_NAMESPACE,
        ],
    },
    HandlerDef {
        name: "tool.check",
        description: "Policy decision for an actor calling a tool: allow, deny or ask, with its source (grant, policy or the side-effect default).",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[P_TOOL, P_ACTOR, P_NAMESPACE],
    },
    HandlerDef {
        name: "tool.request",
        description: "Ask for approval to call a tool. Returns allow at once when policy already allows; otherwise records a grant request (and optionally notifies an approver) and returns ask or deny.",
        visibility: Visibility::Verb,
        category: VerbCategory::Directive,
        params: &[
            P_TOOL,
            P_ACTOR,
            ParamDef { name: "scope", param_type: "string", required: false, description: "What the grant should cover (free text, stored with the request).", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "reason", param_type: "string", required: false, description: "Why the caller needs it.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "notify", param_type: "string", required: false, description: "Approver actor label to mail through comm.send when the comm pack is loaded.", resolution_mode: IdResolutionMode::NotApplicable },
            P_NAMESPACE,
        ],
    },
    HandlerDef {
        name: "tool.grant",
        description: "Approve a grant request, optionally with an expiry.",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            ParamDef { name: "id", param_type: "string", required: true, description: "Grant request id.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "expires_in_s", param_type: "integer", required: false, description: "Seconds until the grant expires (default: never).", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "note", param_type: "string", required: false, description: "Decision note.", resolution_mode: IdResolutionMode::NotApplicable },
            P_NAMESPACE,
        ],
    },
    HandlerDef {
        name: "tool.deny",
        description: "Deny a grant request.",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            ParamDef { name: "id", param_type: "string", required: true, description: "Grant request id.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "note", param_type: "string", required: false, description: "Decision note.", resolution_mode: IdResolutionMode::NotApplicable },
            P_NAMESPACE,
        ],
    },
    HandlerDef {
        name: "tool.revoke",
        description: "Revoke a granted request.",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            ParamDef { name: "id", param_type: "string", required: true, description: "Grant id.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "note", param_type: "string", required: false, description: "Decision note.", resolution_mode: IdResolutionMode::NotApplicable },
            P_NAMESPACE,
        ],
    },
    HandlerDef {
        name: "tool.requests",
        description: "List grant requests and grants, newest first.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef { name: "status", param_type: "string", required: false, description: "requested, granted, denied or revoked.", resolution_mode: IdResolutionMode::NotApplicable },
            P_ACTOR,
            ParamDef { name: "tool", param_type: "string", required: false, description: "Restrict to one tool name.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "limit", param_type: "integer", required: false, description: "Page size (default 50).", resolution_mode: IdResolutionMode::NotApplicable },
            P_NAMESPACE,
        ],
    },
    HandlerDef {
        name: "tool.policy",
        description: "Set a policy row: actor pattern, tool pattern (exact, prefix with a trailing *, or *), decision allow, deny or ask. The most specific matching row wins; on ties deny beats ask beats allow.",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef { name: "actor", param_type: "string", required: true, description: "Actor label or pattern.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "tool", param_type: "string", required: true, description: "Tool name or pattern.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "decision", param_type: "string", required: true, description: "allow, deny or ask.", resolution_mode: IdResolutionMode::NotApplicable },
            ParamDef { name: "note", param_type: "string", required: false, description: "Why.", resolution_mode: IdResolutionMode::NotApplicable },
            P_NAMESPACE,
        ],
    },
    HandlerDef {
        name: "tool.policies",
        description: "List policy rows.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            P_ACTOR,
            ParamDef { name: "limit", param_type: "integer", required: false, description: "Page size (default 100).", resolution_mode: IdResolutionMode::NotApplicable },
            P_NAMESPACE,
        ],
    },
];
