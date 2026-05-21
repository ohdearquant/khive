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
use khive_runtime::{KhiveRuntime, KindHook, RuntimeError, VerbRegistry};
use khive_types::{EdgeEndpointRule, EdgeRelation, EndpointKind, Pack, VerbDef};

use crate::hook::TaskHook;

/// GTD pack — registers the `task` note kind plus five lifecycle verbs.
pub struct GtdPack {
    runtime: KhiveRuntime,
}

impl Pack for GtdPack {
    const NAME: &'static str = "gtd";
    const NOTE_KINDS: &'static [&'static str] = &["task"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const VERBS: &'static [VerbDef] = &GTD_VERBS;
    const EDGE_RULES: &'static [EdgeEndpointRule] = &GTD_EDGE_RULES;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

/// ADR-031: GTD opts task notes into `depends_on` between tasks. The base
/// ADR-002 contract keeps `depends_on` as entity→entity for KG semantics;
/// this rule additively extends it to task→task so blockers are graph-traversable.
static GTD_EDGE_RULES: [EdgeEndpointRule; 1] = [EdgeEndpointRule {
    relation: EdgeRelation::DependsOn,
    source: EndpointKind::NoteOfKind("task"),
    target: EndpointKind::NoteOfKind("task"),
}];

// ADR-060: Illocutionary classification (Searle 1976)
//   Directive — attempts to get hearer to do something
//   Assertive — retrieves/presents state of affairs
//   Declaration — changes institutional status by fiat
static GTD_VERBS: [VerbDef; 5] = [
    // Directive: directs an actor to perform work
    VerbDef {
        name: "assign",
        description: "Create a GTD task (note with kind=task)",
    },
    // Assertive: retrieves actionable tasks
    VerbDef {
        name: "next",
        description: "List actionable tasks (status=next or active) by priority",
    },
    // Declaration: declares a task done
    VerbDef {
        name: "complete",
        description: "Mark a task done with an optional result note",
    },
    // Assertive: retrieves filtered task listing
    VerbDef {
        name: "tasks",
        description: "List tasks filtered by status, assignee, priority",
    },
    // Declaration: changes task lifecycle status
    VerbDef {
        name: "transition",
        description: "Explicit GTD status transition with lifecycle validation",
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

// ── ADR-063: inventory self-registration ─────────────────────────────────────

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

    fn verbs(&self) -> &'static [VerbDef] {
        &GTD_VERBS
    }

    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        <GtdPack as Pack>::EDGE_RULES
    }

    fn requires(&self) -> &'static [&'static str] {
        <GtdPack as Pack>::REQUIRES
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
    ) -> Result<Value, RuntimeError> {
        match verb {
            "assign" => self.handle_assign(params).await,
            "next" => self.handle_next(params).await,
            "complete" => self.handle_complete(params).await,
            "tasks" => self.handle_tasks(params).await,
            "transition" => self.handle_transition(params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "gtd pack does not handle verb {verb:?}"
            ))),
        }
    }
}
