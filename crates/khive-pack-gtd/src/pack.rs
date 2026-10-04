//! `GtdPack` struct, `Pack` impl, self-registration factory, and `PackRuntime` impl.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    KhiveRuntime, KindHook, NamespaceToken, NoteKindSpec, PackSchemaPlan, RuntimeError,
    VerbRegistry,
};
use khive_types::{EdgeEndpointRule, HandlerDef, Pack};

use crate::hook::TaskHook;
use crate::vocab::{GTD_EDGE_RULES, GTD_HANDLERS, GTD_NOTE_KIND_SPECS, GTD_SCHEMA_PLAN_STMTS};

/// GTD pack — task lifecycle, timestamp census, and explicit repair.
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
    khive_runtime::pack_factory_metadata!(GtdPack);

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(GtdPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&GtdPackFactory) }

#[async_trait]
impl PackRuntime for GtdPack {
    khive_runtime::pack_runtime_metadata!();

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
            "gtd.census" => self.handle_census(token, params).await,
            "gtd.repair" => self.handle_repair(token, params).await,
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

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::PackFactory;

    #[test]
    fn factory_metadata_matches_the_pack_it_constructs() {
        let factory = GtdPackFactory;
        assert_eq!(factory.name(), <GtdPack as Pack>::NAME);
        assert_eq!(factory.requires(), <GtdPack as Pack>::REQUIRES);
    }
}
