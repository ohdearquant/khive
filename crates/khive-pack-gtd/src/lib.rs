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
use khive_types::{Pack, VerbDef};

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
}

static GTD_VERBS: [VerbDef; 5] = [
    VerbDef {
        name: "assign",
        description: "Create a GTD task (note with kind=task)",
    },
    VerbDef {
        name: "next",
        description: "List actionable tasks (status=next or active) by priority",
    },
    VerbDef {
        name: "complete",
        description: "Mark a task done with an optional result note",
    },
    VerbDef {
        name: "tasks",
        description: "List tasks filtered by status, assignee, priority",
    },
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
