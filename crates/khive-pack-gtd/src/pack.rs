//! `GtdPack` struct, `Pack` impl, self-registration factory, and `PackRuntime` impl.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    KhiveRuntime, KindHook, NamespaceToken, NoteKindSpec, PackSchemaPlan, RuntimeError, SchemaPlan,
    VerbRegistry,
};
use khive_types::{EdgeEndpointRule, HandlerDef, Pack};

use crate::hook::TaskHook;
use crate::vocab::{GTD_EDGE_RULES, GTD_HANDLERS, GTD_NOTE_KIND_SPECS, GTD_SCHEMA_PLAN_STMTS};

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

impl GtdPack {
    /// Create a new `GtdPack` bound to the given runtime.
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }

    pub(crate) fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }
}

// ── inventory self-registration ───────────────────────────────────────────────

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
            "gtd.assign" => self.handle_assign(token, params).await,
            "gtd.next" => self.handle_next(token, params).await,
            "gtd.complete" => self.handle_complete(token, params).await,
            "gtd.tasks" => self.handle_tasks(token, params).await,
            "gtd.transition" => self.handle_transition(token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "gtd pack does not handle verb {verb:?}"
            ))),
        }
    }
}
