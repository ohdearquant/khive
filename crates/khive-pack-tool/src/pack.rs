//! `ToolPack` self-registration factory and `PackRuntime` dispatch impl.

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, SchemaPlan, VerbRegistry};
use khive_types::{EdgeEndpointRule, EntityTypeDef, HandlerDef, Pack, PackSchemaPlan};

use crate::handlers;
use crate::vocab::{ENTITY_TYPES, PACK_NAME, TOOL_HANDLERS, TOOL_SCHEMA_PLAN_STMTS};

pub struct ToolPack {
    runtime: KhiveRuntime,
}

impl ToolPack {
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }

    pub(crate) fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }
}

impl Pack for ToolPack {
    const NAME: &'static str = PACK_NAME;
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &TOOL_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["kg"];
    const ENTITY_TYPES: &'static [EntityTypeDef] = &ENTITY_TYPES;
    const SCHEMA_PLAN: Option<PackSchemaPlan> = Some(PackSchemaPlan {
        pack: PACK_NAME,
        statements: &TOOL_SCHEMA_PLAN_STMTS,
    });
}

struct ToolPackFactory;

impl khive_runtime::PackFactory for ToolPackFactory {
    fn name(&self) -> &'static str {
        PACK_NAME
    }

    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(ToolPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&ToolPackFactory) }

#[async_trait]
impl PackRuntime for ToolPack {
    fn name(&self) -> &str {
        <ToolPack as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <ToolPack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <ToolPack as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        <ToolPack as Pack>::HANDLERS
    }

    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        &[]
    }

    fn entity_types(&self) -> &'static [EntityTypeDef] {
        <ToolPack as Pack>::ENTITY_TYPES
    }

    fn requires(&self) -> &'static [&'static str] {
        <ToolPack as Pack>::REQUIRES
    }

    fn schema_plan(&self) -> SchemaPlan {
        SchemaPlan {
            pack: PACK_NAME,
            statements: &TOOL_SCHEMA_PLAN_STMTS,
        }
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        let rt = self.runtime();
        match verb {
            "tool.register" => handlers::register(rt, token, params).await,
            "tool.ingest" => handlers::ingest(rt, registry, token, params).await,
            "tool.suggest" => handlers::suggest(rt, token, params).await,
            "tool.describe" => handlers::describe(rt, token, params).await,
            "tool.list" => handlers::list(rt, token, params).await,
            "tool.check" => handlers::check(rt, token, params).await,
            "tool.request" => handlers::request(rt, registry, token, params).await,
            "tool.grant" => handlers::decide_request(rt, token, params, "granted").await,
            "tool.deny" => handlers::decide_request(rt, token, params, "denied").await,
            "tool.revoke" => handlers::decide_request(rt, token, params, "revoked").await,
            "tool.requests" => handlers::requests(rt, token, params).await,
            "tool.policy" => handlers::set_policy(rt, token, params).await,
            "tool.policies" => handlers::policies(rt, token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "{PACK_NAME} pack does not handle verb {verb:?}"
            ))),
        }
    }
}
