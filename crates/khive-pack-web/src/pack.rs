use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, SchemaPlan, VerbRegistry};
use khive_types::{EdgeEndpointRule, HandlerDef, Pack};

use crate::vocab::{WEB_EDGE_RULES, WEB_HANDLERS};

/// Opt-in site vocabulary and local manifest ingest into a dedicated map database.
pub struct WebPack {
    pub(crate) runtime: KhiveRuntime,
}

impl Pack for WebPack {
    const NAME: &'static str = "web";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &WEB_HANDLERS;
    const EDGE_RULES: &'static [EdgeEndpointRule] = &WEB_EDGE_RULES;
    const REQUIRES: &'static [&'static str] = &["kg"];
    const SCHEMA_PLAN: Option<khive_runtime::PackSchemaPlan> = None;
}

impl WebPack {
    /// Bind the web pack to the caller's runtime.
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }
}

struct WebPackFactory;

impl khive_runtime::PackFactory for WebPackFactory {
    fn name(&self) -> &'static str {
        <WebPack as Pack>::NAME
    }

    fn requires(&self) -> &'static [&'static str] {
        <WebPack as Pack>::REQUIRES
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        Box::new(WebPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&WebPackFactory) }

#[async_trait]
impl PackRuntime for WebPack {
    fn name(&self) -> &str {
        <WebPack as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <WebPack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <WebPack as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        <WebPack as Pack>::HANDLERS
    }

    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        <WebPack as Pack>::EDGE_RULES
    }

    fn requires(&self) -> &'static [&'static str] {
        <WebPack as Pack>::REQUIRES
    }

    fn schema_plan(&self) -> SchemaPlan {
        SchemaPlan {
            pack: "web",
            statements: &[],
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
            "web.ingest" => self.handle_ingest(params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "web pack does not handle verb {verb:?}"
            ))),
        }
    }
}
