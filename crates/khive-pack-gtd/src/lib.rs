//! pack-gtd — GTD (Getting Things Done) verb pack for khive.
//!
//! Adds a single `task` note kind plus five verbs (`assign`, `next`,
//! `complete`, `tasks`, `transition`) that wrap the notes substrate with
//! GTD lifecycle semantics:
//!
//! ```text
//! inbox → next | waiting | someday | active | done | cancelled
//! next  → active | waiting | someday | done | cancelled
//! ...
//! ```
//!
//! Status, priority, assignee, due/start/end, depends_on and tags live in
//! `note.properties` — no new schema migration is required.

pub mod handlers;
pub mod hook;
pub mod schema;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    KhiveRuntime, KindHook, NamespaceToken, NoteKindSpec, NoteLifecycleSpec, PackSchemaPlan,
    RuntimeError, SchemaPlan, VerbRegistry,
};
use khive_types::{
    EdgeEndpointRule, EdgeRelation, EndpointKind, HandlerDef, Pack, ParamDef, VerbCategory,
    Visibility,
};

use crate::hook::TaskHook;

/// GTD pack — registers the `task` note kind plus five lifecycle verbs.
pub struct GtdPack {
    runtime: KhiveRuntime,
}

impl Pack for GtdPack {
    const NAME: &'static str = "gtd";
    const NOTE_KINDS: &'static [&'static str] = &["task"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &GTD_HANDLERS;
    const EDGE_RULES: &'static [EdgeEndpointRule] = &GTD_EDGE_RULES;
    const REQUIRES: &'static [&'static str] = &["kg"];
    const NOTE_KIND_SPECS: &'static [NoteKindSpec] = &GTD_NOTE_KIND_SPECS;
    const SCHEMA_PLAN: Option<PackSchemaPlan> = Some(PackSchemaPlan {
        pack: "gtd",
        statements: &GTD_SCHEMA_PLAN_STMTS,
    });
}

/// ADR-031: GTD opts task notes into `depends_on` between tasks. The base
/// ADR-002 contract keeps `depends_on` as entity→entity for KG semantics;
/// this rule additively extends it to task→task so blockers are graph-traversable.
static GTD_EDGE_RULES: [EdgeEndpointRule; 1] = [EdgeEndpointRule {
    relation: EdgeRelation::DependsOn,
    source: EndpointKind::NoteOfKind("task"),
    target: EndpointKind::NoteOfKind("task"),
}];

/// ADR-004 §NoteKindSpec: lifecycle declaration for the `task` note kind.
///
/// The lifecycle field is named `kind_status` (not `properties["status"]`) to
/// avoid the semantic collision with `Note.status` (NoteStatus visibility).
///
/// Phase 1: this spec is declared and collected by the runtime for introspection
/// and documentation.  The `task` note kind currently stores lifecycle state in
/// `properties["status"]` (status quo); Phase 2 will migrate to a first-class
/// `kind_status` column once the runtime enforcement layer is in place (c11/c12).
static GTD_NOTE_KIND_SPECS: [NoteKindSpec; 1] = [NoteKindSpec {
    kind: "task",
    aliases: &["todo", "issue"],
    lifecycle: NoteLifecycleSpec {
        // ADR-004: lifecycle field name must NOT be "status" to avoid collision
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

/// ADR-019 §schema_plan: pack-auxiliary schema for GTD lifecycle audit.
///
/// `gtd_lifecycle_audit` records every `transition` (and `complete`) invocation
/// for replay and compliance auditing.  The table is idempotent (`CREATE TABLE
/// IF NOT EXISTS`) and is NOT part of the core versioned migration chain.
pub(crate) static GTD_SCHEMA_PLAN_STMTS: [&str; 2] = [
    "CREATE TABLE IF NOT EXISTS gtd_lifecycle_audit (\
        note_id    TEXT NOT NULL,\
        from_state TEXT NOT NULL,\
        to_state   TEXT NOT NULL,\
        note       TEXT,\
        at         INTEGER NOT NULL\
    )",
    "CREATE INDEX IF NOT EXISTS idx_gtd_audit_note \
        ON gtd_lifecycle_audit(note_id, at DESC)",
];

// ADR-025: Illocutionary classification (Searle 1976)
//   Directive  — attempts to get hearer to do something
//   Assertive  — retrieves/presents state of affairs
//   Declaration — changes institutional status by fiat
static GTD_HANDLERS: [HandlerDef; 5] = [
    // Directive: directs an actor to perform work
    HandlerDef {
        name: "assign",
        description: "Create a GTD task (note with kind=task)",
        visibility: Visibility::Verb,
        category: VerbCategory::Directive,
        params: &[
            ParamDef {
                name: "title",
                param_type: "string",
                required: true,
                description: "Task title.",
            },
            ParamDef {
                name: "status",
                param_type: "string",
                required: false,
                description: "Initial status: inbox | next | waiting | someday | active (default inbox).",
            },
            ParamDef {
                name: "priority",
                param_type: "string",
                required: false,
                description: "Priority: p0 | p1 | p2 | p3 (default p2).",
            },
            ParamDef {
                name: "assignee",
                param_type: "string",
                required: false,
                description: "Assignee identifier.",
            },
            ParamDef {
                name: "due",
                param_type: "string",
                required: false,
                description: "Due date (ISO-8601).",
            },
            ParamDef {
                name: "depends_on",
                param_type: "array of uuid",
                required: false,
                description: "UUIDs of blocking tasks.",
            },
            ParamDef {
                name: "tags",
                param_type: "array of string",
                required: false,
                description: "Tag list.",
            },
        ],
    },
    // Assertive: retrieves actionable tasks
    HandlerDef {
        name: "next",
        description: "List actionable tasks (status=next or active) by priority",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Maximum tasks to return (default 10).",
            },
            ParamDef {
                name: "assignee",
                param_type: "string",
                required: false,
                description: "Filter to this assignee.",
            },
        ],
    },
    // Declaration: declares a task done
    HandlerDef {
        name: "complete",
        description: "Mark a task done with an optional result note",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "id",
                param_type: "uuid",
                required: true,
                description: "UUID of the task to complete.",
            },
            ParamDef {
                name: "result",
                param_type: "string",
                required: false,
                description: "Optional result or completion note.",
            },
        ],
    },
    // Assertive: retrieves filtered task listing
    HandlerDef {
        name: "tasks",
        description: "List tasks filtered by status, assignee, priority",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "status",
                param_type: "string",
                required: false,
                description: "Filter by status: inbox | next | waiting | someday | active | done | cancelled.",
            },
            ParamDef {
                name: "assignee",
                param_type: "string",
                required: false,
                description: "Filter by assignee.",
            },
            ParamDef {
                name: "priority",
                param_type: "string",
                required: false,
                description: "Filter by priority: p0 | p1 | p2 | p3.",
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Maximum results (default 20).",
            },
            ParamDef {
                name: "offset",
                param_type: "integer",
                required: false,
                description: "Pagination offset (default 0).",
            },
        ],
    },
    // Declaration: changes task lifecycle status
    HandlerDef {
        name: "transition",
        description: "Explicit GTD status transition with lifecycle validation",
        visibility: Visibility::Verb,
        category: VerbCategory::Declaration,
        params: &[
            ParamDef {
                name: "id",
                param_type: "uuid",
                required: true,
                description: "UUID of the task to transition.",
            },
            ParamDef {
                name: "status",
                param_type: "string",
                required: true,
                description: "Target status: inbox | next | waiting | someday | active | done | cancelled.",
            },
            ParamDef {
                name: "note",
                param_type: "string",
                required: false,
                description: "Optional note to attach to the transition.",
            },
        ],
    },
];

impl GtdPack {
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }

    pub(crate) fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }
}

// ── ADR-027: inventory self-registration ─────────────────────────────────────

struct GtdPackFactory;

impl khive_runtime::PackFactory for GtdPackFactory {
    fn name(&self) -> &'static str {
        "gtd"
    }

    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(GtdPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&GtdPackFactory) }

#[async_trait]
impl PackRuntime for GtdPack {
    fn name(&self) -> &str {
        <GtdPack as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <GtdPack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <GtdPack as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        &GTD_HANDLERS
    }

    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        <GtdPack as Pack>::EDGE_RULES
    }

    fn requires(&self) -> &'static [&'static str] {
        <GtdPack as Pack>::REQUIRES
    }

    fn note_kind_specs(&self) -> &'static [NoteKindSpec] {
        <GtdPack as Pack>::NOTE_KIND_SPECS
    }

    fn schema_plan(&self) -> SchemaPlan {
        SchemaPlan {
            pack: "gtd",
            statements: &GTD_SCHEMA_PLAN_STMTS,
        }
    }

    fn kind_hook(&self, kind: &str) -> Option<Arc<dyn KindHook>> {
        match kind {
            "task" => Some(Arc::new(TaskHook)),
            _ => None,
        }
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "assign" => self.handle_assign(token, params).await,
            "next" => self.handle_next(token, params).await,
            "complete" => self.handle_complete(token, params).await,
            "tasks" => self.handle_tasks(token, params).await,
            "transition" => self.handle_transition(token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "gtd pack does not handle verb {verb:?}"
            ))),
        }
    }
}
