use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::{KhiveRuntime, NamespaceToken, PackRuntime, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, Pack, PackSchemaPlan};

/// Schema-only foundation for charter recording; no action can be admitted.
pub struct CharterPack;

impl Pack for CharterPack {
    const NAME: &'static str = "charter";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
    const SCHEMA_PLAN: Option<PackSchemaPlan> = Some(PackSchemaPlan {
        pack: Self::NAME,
        statements: crate::schema::CHARTER_SCHEMA_STATEMENTS,
    });
}

impl CharterPack {
    pub fn new(_runtime: KhiveRuntime) -> Self {
        Self
    }
}

struct CharterPackFactory;

impl khive_runtime::PackFactory for CharterPackFactory {
    khive_runtime::pack_factory_metadata!(CharterPack);

    fn intentionally_verbless(&self) -> bool {
        true
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        Box::new(CharterPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&CharterPackFactory) }

#[async_trait]
impl PackRuntime for CharterPack {
    khive_runtime::pack_runtime_metadata!();

    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "charter pack does not handle verb {verb:?}; the recording schema exposes no verbs"
        )))
    }
}
