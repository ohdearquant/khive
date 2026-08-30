//! GTD pack vocabulary: handler definitions, edge rules, note kind specs, and schema plan.

use khive_runtime::{NoteKindSpec, NoteLifecycleSpec};
use khive_types::{
    EdgeEndpointRule, EdgeRelation, EndpointKind, HandlerDef, IdResolutionMode, ParamDef,
    VerbCategory, Visibility,
};

/// GTD opts task notes into `depends_on` between tasks. The base contract keeps
/// `depends_on` as entity→entity for KG semantics; this rule additively extends
/// it to task→task so blockers are graph-traversable.
pub(crate) static GTD_EDGE_RULES: [EdgeEndpointRule; 1] = [EdgeEndpointRule {
    relation: EdgeRelation::DependsOn,
    source: EndpointKind::NoteOfKind("task"),
    target: EndpointKind::NoteOfKind("task"),
}];

/// Lifecycle declaration for the `task` note kind.
///
/// The lifecycle field is named `kind_status` (not `properties["status"]`) to
/// avoid the semantic collision with `Note.status` (NoteStatus visibility).
///
/// Phase 1: this spec is declared and collected by the runtime for introspection
/// and documentation. The `task` note kind currently stores lifecycle state in
/// `properties["status"]` (status quo); Phase 2 will migrate to a first-class
/// `kind_status` column once the runtime enforcement layer is in place (c11/c12).
pub(crate) static GTD_NOTE_KIND_SPECS: [NoteKindSpec; 1] = [NoteKindSpec {
    kind: "task",
    aliases: &["todo", "issue"],
    lifecycle: NoteLifecycleSpec {
        // Lifecycle field name must NOT be "status" to avoid collision
        // with NoteStatus. The canonical name is "kind_status".
        field: "kind_status",
        initial: "inbox",
        terminal: &["done", "cancelled"],
        transitions: &[
            ("inbox", "next"),
            ("inbox", "waiting"),
            ("inbox", "someday"),
            ("inbox", "active"),
            ("inbox", "done"),
            ("inbox", "cancelled"),
            ("next", "active"),
            ("next", "waiting"),
            ("next", "someday"),
            ("next", "done"),
            ("next", "cancelled"),
            ("active", "next"),
            ("active", "waiting"),
            ("active", "done"),
            ("active", "cancelled"),
            ("waiting", "next"),
            ("waiting", "active"),
            ("waiting", "done"),
            ("waiting", "cancelled"),
            ("someday", "next"),
            ("someday", "active"),
            ("someday", "done"),
            ("someday", "cancelled"),
            // done and cancelled are terminal — no outgoing transitions (#273).
        ],
    },
}];

/// Pack-auxiliary schema for GTD lifecycle audit.
///
/// `gtd_lifecycle_audit` receives best-effort rows for successful real
/// `transition`/`complete` changes and canonical same-status note events. The
/// table is idempotent (`CREATE TABLE IF NOT EXISTS`) and is NOT part of the
/// core versioned migration chain.
///
/// Every statement must be idempotent so the generic boot applier can call them
/// on any database (fresh or pre-existing) without error. The fresh-table DDL
/// includes `namespace`. Older databases that already have
/// `gtd_lifecycle_audit` without that column are upgraded by the guarded
/// `ensure_audit_schema` path in `handlers.rs` (`PRAGMA table_info` check
/// followed by `ALTER TABLE ... ADD COLUMN namespace` only when missing).
pub(crate) static GTD_SCHEMA_PLAN_STMTS: [&str; 2] = [
    "CREATE TABLE IF NOT EXISTS gtd_lifecycle_audit (\
        note_id    TEXT NOT NULL,\
        from_state TEXT NOT NULL,\
        to_state   TEXT NOT NULL,\
        note       TEXT,\
        at         INTEGER NOT NULL,\
        namespace  TEXT\
    )",
    "CREATE INDEX IF NOT EXISTS idx_gtd_audit_note \
        ON gtd_lifecycle_audit(note_id, at DESC)",
];

/// Illocutionary classification (Searle 1976):
///   Directive  — attempts to get hearer to do something
///   Assertive  — retrieves/presents state of affairs
///   Declaration — changes institutional status by fiat
pub(crate) static GTD_HANDLERS: [HandlerDef; 5] = [
    // Directive: directs an actor to perform work
    HandlerDef {
        name: "gtd.assign",
        description: "Create a GTD task (note with kind=task)",
        visibility: Visibility::Verb,
        category: VerbCategory::Directive,
        params: &[
            ParamDef {
                name: "title",
                param_type: "string",
                required: true,
                description: "Task title.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "status",
                param_type: "string",
                required: false,
                description: "Initial status: inbox | next | waiting | someday | active (default inbox). \
                               Canonical values also accepted as aliases: todo=inbox, in_progress=active, \
                               blocked=waiting, later=someday, finished=done.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "priority",
                param_type: "string",
                required: false,
                description: "Priority: p0 | p1 | p2 | p3 (default p2).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "assignee",
                param_type: "string",
                required: false,
                description: "Assignee identifier.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "due",
                param_type: "string",
                required: false,
                description: "Due date (ISO-8601).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "depends_on",
                param_type: "array of uuid",
                required: false,
                description: "Complete UUIDs or unique 8+ hex prefixes of blocking task notes. \
                              Prefix resolution searches the caller's primary namespace, and \
                              each resolved record must be a task in that namespace.",
                resolution_mode: IdResolutionMode::FullAndPrefixScopedToPrimary,
            },
            ParamDef {
                name: "context_entity_id",
                param_type: "uuid",
                required: false,
                description: "Full UUID of the KG entity this task concerns. A short prefix would require primary-namespace resolution and is rejected because this field stores an explicit stable reference.",
                resolution_mode: IdResolutionMode::FullUuidOnlyScopedToPrimary,
            },
            ParamDef {
                name: "tags",
                param_type: "array of string",
                required: false,
                description: "Tag list.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // Assertive: retrieves actionable tasks
    HandlerDef {
        name: "gtd.next",
        description: "List actionable tasks (status=next or active) by priority",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Maximum tasks to return (default 10, silently clamped to 200 — issue #744: a `limit` above 200 is capped without a separate signal in the response).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "assignee",
                param_type: "string",
                required: false,
                description: "Filter to this assignee.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "include_blocked",
                param_type: "boolean",
                required: false,
                description: "Include status=next/active tasks that are blocked. Returned tasks carry actionable, dependency_state, and blocked_by diagnostics (default false).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // Declaration: declares a task done or cancelled
    HandlerDef {
        name: "gtd.complete",
        description: "Mark a task done (or cancelled) with an optional result note",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "id",
                param_type: "uuid",
                required: true,
                description: "Full UUID or unique 8+ hex prefix of the task to complete. By-ID resolution is namespace-agnostic (ADR-007).",
                resolution_mode: IdResolutionMode::UnscopedById,
            },
            ParamDef {
                name: "result",
                param_type: "string",
                required: false,
                description: "Optional result or completion note.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "status",
                param_type: "string",
                required: false,
                description: "Terminal status: \"done\" (default) or \"cancelled\".",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // Assertive: retrieves filtered task listing
    HandlerDef {
        name: "gtd.tasks",
        description: "List tasks filtered by status, assignee, priority. DEFAULT (no `status` \
                       given): excludes terminal statuses (done, cancelled) so the default \
                       listing shows only active work. Pass status=\"done\" or \
                       status=\"cancelled\" explicitly to see completed/cancelled tasks — an \
                       empty result under the default filter does NOT mean the task doesn't \
                       exist; the response carries `filter_excluded` when the default filter \
                       is the reason the result is empty (issue #96).",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "status",
                param_type: "string",
                required: false,
                description: "Filter by status: inbox | next | waiting | someday | active | done | cancelled. \
                               Aliases also accepted: todo=inbox, in_progress=active, blocked=waiting, \
                               later=someday, finished=done. Omitted (default): excludes done \
                               and cancelled (terminal statuses) from the result.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "assignee",
                param_type: "string",
                required: false,
                description: "Filter by assignee.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "priority",
                param_type: "string",
                required: false,
                description: "Filter by priority: p0 | p1 | p2 | p3.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Maximum results (default 20, silently clamped to 200 — issue #744: a `limit` above 200 is capped without a separate signal in the response).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "offset",
                param_type: "integer",
                required: false,
                description: "Pagination offset (default 0).",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    // Declaration: changes task lifecycle status
    HandlerDef {
        name: "gtd.transition",
        description: "Explicit GTD status transition with lifecycle validation",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "id",
                param_type: "uuid",
                required: true,
                description: "Full UUID or unique 8+ hex prefix of the task to transition. By-ID resolution is namespace-agnostic (ADR-007).",
                resolution_mode: IdResolutionMode::UnscopedById,
            },
            ParamDef {
                name: "status",
                param_type: "string",
                required: true,
                description: "Target status: inbox | next | waiting | someday | active | done | cancelled. \
                               Aliases also accepted: todo=inbox, in_progress=active, blocked=waiting, \
                               later=someday, finished=done.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "note",
                param_type: "string",
                required: false,
                description: "Optional note to attach to the transition.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
];
