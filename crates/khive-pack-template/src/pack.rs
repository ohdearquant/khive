//! Handler table, inventory registration, and runtime dispatch for the template pack.

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, IdResolutionMode, ParamDef, Visibility};

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
        resolution_mode: IdResolutionMode::NotApplicable,
    }],
}];

struct TemplatePackFactory;

impl khive_runtime::PackFactory for TemplatePackFactory {
    khive_runtime::pack_factory_metadata!(TemplatePack);
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(TemplatePack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&TemplatePackFactory) }

#[async_trait]
impl PackRuntime for TemplatePack {
    khive_runtime::pack_runtime_metadata!();

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
