//! `CodePack` struct, `Pack` impl, self-registration factory, and `PackRuntime` impl.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    KhiveRuntime, KindHook, NamespaceToken, NoteKindSpec, RuntimeError, SchemaPlan, VerbRegistry,
};
use khive_types::{EdgeEndpointRule, HandlerDef, Pack};

use crate::hook::FindingHook;
use crate::vocab::{CODE_EDGE_RULES, CODE_HANDLERS, CODE_NOTE_KIND_SPECS};

/// Code ontology pack — additive edge rules over four concept subtypes, the
/// `finding` audit-observation note kind, and the `code.ingest` verb
/// (ADR-085 Amendment 2).
///
/// See `crates/khive-pack-code/docs/code-ontology.md`.
pub struct CodePack {
    pub(crate) runtime: KhiveRuntime,
}

impl Pack for CodePack {
    const NAME: &'static str = "code";
    const NOTE_KINDS: &'static [&'static str] = &["finding"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &CODE_HANDLERS;
    const EDGE_RULES: &'static [EdgeEndpointRule] = &CODE_EDGE_RULES;
    const REQUIRES: &'static [&'static str] = &["kg"];
    const NOTE_KIND_SPECS: &'static [NoteKindSpec] = &CODE_NOTE_KIND_SPECS;
    const SCHEMA_PLAN: Option<khive_runtime::PackSchemaPlan> = None;
}

impl CodePack {
    /// Bind the code vocabulary pack to `runtime`; the pack registers no verbs.
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }
}

struct CodePackFactory;

impl khive_runtime::PackFactory for CodePackFactory {
    fn name(&self) -> &'static str {
        "code"
    }

    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(CodePack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&CodePackFactory) }

#[async_trait]
impl PackRuntime for CodePack {
    fn name(&self) -> &str {
        <CodePack as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <CodePack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <CodePack as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        <CodePack as Pack>::HANDLERS
    }

    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        <CodePack as Pack>::EDGE_RULES
    }

    fn requires(&self) -> &'static [&'static str] {
        <CodePack as Pack>::REQUIRES
    }

    fn note_kind_specs(&self) -> &'static [NoteKindSpec] {
        <CodePack as Pack>::NOTE_KIND_SPECS
    }

    fn schema_plan(&self) -> SchemaPlan {
        SchemaPlan {
            pack: "code",
            statements: &[],
        }
    }

    fn kind_hook(&self, kind: &str) -> Option<Arc<dyn KindHook>> {
        match kind {
            "finding" => Some(Arc::new(FindingHook)),
            _ => None,
        }
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "code.ingest" => self.handle_ingest(params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "code pack does not handle verb {verb:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use khive_types::Pack;

    use super::CodePack;

    #[test]
    fn code_pack_declares_adr085_metadata() {
        assert_eq!(<CodePack as Pack>::NAME, "code");
        assert_eq!(<CodePack as Pack>::NOTE_KINDS, &["finding"]);
        assert!(<CodePack as Pack>::ENTITY_KINDS.is_empty());
        assert_eq!(<CodePack as Pack>::HANDLERS.len(), 1);
        assert_eq!(<CodePack as Pack>::HANDLERS[0].name, "code.ingest");
        assert_eq!(<CodePack as Pack>::REQUIRES, &["kg"]);
        assert!(<CodePack as Pack>::SCHEMA_PLAN.is_none());
    }
}
