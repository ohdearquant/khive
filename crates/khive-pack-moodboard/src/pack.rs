//! Inventory registration and runtime dispatch for the opt-in Moodboard pack.

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};

use crate::{handlers, preference_handlers, MoodboardPack, PACK_NAME};

struct MoodboardPackFactory;

impl khive_runtime::PackFactory for MoodboardPackFactory {
    khive_runtime::pack_factory_metadata!(MoodboardPack);

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        Box::new(MoodboardPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&MoodboardPackFactory) }

#[async_trait]
impl PackRuntime for MoodboardPack {
    khive_runtime::pack_runtime_metadata!();

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
            "moodboard.serve" => preference_handlers::handle_serve(self, token, params).await,
            "moodboard.judge" => preference_handlers::handle_judge(self, token, params).await,
            "moodboard.train_preference" => {
                preference_handlers::handle_train_preference(self, token, params).await
            }
            "moodboard.preference" => {
                preference_handlers::handle_preference(self, token, params).await
            }
            _ => Err(RuntimeError::InvalidInput(format!(
                "{PACK_NAME} pack does not handle verb {verb:?}"
            ))),
        }
    }
}
