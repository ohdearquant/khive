//! Formal ontology pack registration and runtime adapter.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    KhiveRuntime, KindHook, NamespaceToken, NoteKindSpec, RuntimeError, VerbRegistry,
};
use khive_types::{EdgeEndpointRule, HandlerDef, Pack};

use crate::vocab::FORMAL_EDGE_RULES;

/// Formal-mathematics edge-rule extension with no verbs or private schema.
///
/// See `crates/khive-pack-formal/docs/api/formal-edge-rules.md`.
pub struct FormalPack {
    #[allow(dead_code)]
    runtime: KhiveRuntime,
}

impl Pack for FormalPack {
    const NAME: &'static str = "formal";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
    const EDGE_RULES: &'static [EdgeEndpointRule] = &FORMAL_EDGE_RULES;
    const REQUIRES: &'static [&'static str] = &["kg"];
    const NOTE_KIND_SPECS: &'static [NoteKindSpec] = &[];
    const SCHEMA_PLAN: Option<khive_runtime::PackSchemaPlan> =
        Some(khive_runtime::PackSchemaPlan {
            pack: "formal",
            statements: &[],
        });
}

impl FormalPack {
    /// Bind the vocabulary-only formal pack to `runtime`.
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }
}

struct FormalPackFactory;

impl khive_runtime::PackFactory for FormalPackFactory {
    khive_runtime::pack_factory_metadata!(FormalPack);

    fn intentionally_verbless(&self) -> bool {
        true
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(FormalPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&FormalPackFactory) }

#[async_trait]
impl PackRuntime for FormalPack {
    khive_runtime::pack_runtime_metadata!();

    fn kind_hook(&self, _kind: &str) -> Option<Arc<dyn KindHook>> {
        None
    }

    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "formal pack does not handle verb {verb:?}"
        )))
    }
}

#[cfg(test)]
#[path = "pack_tests.rs"]
mod tests;
