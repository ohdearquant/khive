//! Handler table, inventory registration, and runtime dispatch for the template pack.

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, ParamDef, Visibility};

use crate::{handlers, TemplatePack, PACK_NAME};

/// Example public handler table; one definition is required per dispatchable verb.
///
/// See `crates/khive-pack-template/docs/api/pack-scaffold.md`.
pub(crate) static TEMPLATE_HANDLERS: [HandlerDef; 1] = [HandlerDef {
    name: "template.my_verb",
    description: "Example pack-prefixed verb. Non-kg packs must use pack.verb naming.",
    visibility: Visibility::Verb,
    category: khive_types::VerbCategory::Directive,
    params: &[ParamDef {
        name: "name",
        param_type: "string",
        required: true,
        description: "Non-empty string field to echo in the template response.",
    }],
}];

struct TemplatePackFactory;

impl khive_runtime::PackFactory for TemplatePackFactory {
    fn name(&self) -> &'static str {
        PACK_NAME
    }
    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(TemplatePack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&TemplatePackFactory) }

#[async_trait]
impl PackRuntime for TemplatePack {
    fn name(&self) -> &str {
        <TemplatePack as khive_types::Pack>::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        <TemplatePack as khive_types::Pack>::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        <TemplatePack as khive_types::Pack>::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        &TEMPLATE_HANDLERS
    }
    fn requires(&self) -> &'static [&'static str] {
        <TemplatePack as khive_types::Pack>::REQUIRES
    }

    /// Dispatch a declared verb or return invalid-input for an unknown name.
    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "template.my_verb" => handlers::handle_my_verb(self.runtime(), token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "{PACK_NAME} pack does not handle verb {verb:?}"
            ))),
        }
    }
}
