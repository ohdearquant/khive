//! Inventory registration and runtime dispatch for the opt-in Moodboard pack.

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{EntityTypeDef, HandlerDef, Pack};

use crate::{handlers, MoodboardPack, PACK_NAME};

struct MoodboardPackFactory;

impl khive_runtime::PackFactory for MoodboardPackFactory {
    fn name(&self) -> &'static str {
        PACK_NAME
    }

    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        Box::new(MoodboardPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&MoodboardPackFactory) }

#[async_trait]
impl PackRuntime for MoodboardPack {
    fn name(&self) -> &str {
        <Self as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <Self as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <Self as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        <Self as Pack>::HANDLERS
    }

    fn entity_types(&self) -> &'static [EntityTypeDef] {
        <Self as Pack>::ENTITY_TYPES
    }

    fn requires(&self) -> &'static [&'static str] {
        <Self as Pack>::REQUIRES
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "moodboard.model" => handlers::handle_model(self, params).await,
            "moodboard.ingest" => handlers::handle_ingest(self, token, params).await,
            "moodboard.search" => handlers::handle_search(self, token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "{PACK_NAME} pack does not handle verb {verb:?}"
            ))),
        }
    }
}
