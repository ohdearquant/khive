//! Formal ontology pack registration and runtime adapter.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    KhiveRuntime, KindHook, NamespaceToken, NoteKindSpec, RuntimeError, SchemaPlan, VerbRegistry,
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
    const SCHEMA_PLAN: Option<khive_runtime::PackSchemaPlan> = None;
}

impl FormalPack {
    /// Bind the vocabulary-only formal pack to `runtime`.
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }
}

struct FormalPackFactory;

impl khive_runtime::PackFactory for FormalPackFactory {
    fn name(&self) -> &'static str {
        "formal"
    }

    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(FormalPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&FormalPackFactory) }

#[async_trait]
impl PackRuntime for FormalPack {
    fn name(&self) -> &str {
        <FormalPack as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <FormalPack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <FormalPack as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        <FormalPack as Pack>::HANDLERS
    }

    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        <FormalPack as Pack>::EDGE_RULES
    }

    fn requires(&self) -> &'static [&'static str] {
        <FormalPack as Pack>::REQUIRES
    }

    fn note_kind_specs(&self) -> &'static [NoteKindSpec] {
        <FormalPack as Pack>::NOTE_KIND_SPECS
    }

    fn schema_plan(&self) -> SchemaPlan {
        SchemaPlan {
            pack: "formal",
            statements: &[],
        }
    }

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
mod tests {
    use khive_types::{EdgeRelation, EndpointKind};

    use super::FORMAL_EDGE_RULES;

    #[test]
    fn formal_edge_rules_contains_variant_of_theorem_to_theorem() {
        let found = FORMAL_EDGE_RULES.iter().any(|r| {
            r.relation == EdgeRelation::VariantOf
                && r.source
                    == EndpointKind::EntityOfType {
                        kind: "concept",
                        entity_type: "theorem",
                    }
                && r.target
                    == EndpointKind::EntityOfType {
                        kind: "concept",
                        entity_type: "theorem",
                    }
        });
        assert!(
            found,
            "FORMAL_EDGE_RULES must contain variant_of theorem->theorem"
        );
    }

    #[test]
    fn formal_edge_rules_contains_depends_on_goal_to_theorem() {
        let found = FORMAL_EDGE_RULES.iter().any(|r| {
            r.relation == EdgeRelation::DependsOn
                && r.source
                    == EndpointKind::EntityOfType {
                        kind: "concept",
                        entity_type: "goal",
                    }
                && r.target
                    == EndpointKind::EntityOfType {
                        kind: "concept",
                        entity_type: "theorem",
                    }
        });
        assert!(
            found,
            "FORMAL_EDGE_RULES must contain depends_on goal->theorem"
        );
    }
}
