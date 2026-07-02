//! `SessionPack` self-registration factory and `PackRuntime` dispatch impl.

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, SchemaPlan, VerbRegistry};
use khive_types::{EdgeEndpointRule, HandlerDef, Pack, PackSchemaPlan};

use crate::{handlers, vocab::SESSION_HANDLERS};

pub struct SessionPack {
    runtime: KhiveRuntime,
}

impl SessionPack {
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }

    pub(crate) fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }
}

impl Pack for SessionPack {
    const NAME: &'static str = "session";
    const NOTE_KINDS: &'static [&'static str] = &["session"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &SESSION_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["kg"];
    const SCHEMA_PLAN: Option<PackSchemaPlan> = Some(PackSchemaPlan {
        pack: "session",
        statements: &crate::vocab::SESSION_SCHEMA_PLAN_STMTS,
    });
}

struct SessionPackFactory;

impl khive_runtime::PackFactory for SessionPackFactory {
    fn name(&self) -> &'static str {
        "session"
    }

    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(SessionPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&SessionPackFactory) }

#[async_trait]
impl PackRuntime for SessionPack {
    fn name(&self) -> &str {
        <SessionPack as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <SessionPack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <SessionPack as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        <SessionPack as Pack>::HANDLERS
    }

    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        &[]
    }

    fn requires(&self) -> &'static [&'static str] {
        <SessionPack as Pack>::REQUIRES
    }

    fn schema_plan(&self) -> SchemaPlan {
        SchemaPlan {
            pack: "session",
            statements: &crate::vocab::SESSION_SCHEMA_PLAN_STMTS,
        }
    }

    async fn warm(&self) {
        let config = crate::mirror::MirrorConfig::from_env();
        if !config.enabled && !config.codex_enabled {
            return;
        }
        let runtime = self.runtime().clone();
        tokio::spawn(async move {
            crate::mirror::run_mirror_service(runtime, config).await;
        });
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        let runtime = self.runtime();
        match verb {
            "session.store" => handlers::store::handle_store(runtime, token, params).await,
            "session.list" => handlers::list::handle_list(runtime, token, params).await,
            "session.resume" => handlers::resume::handle_resume(runtime, token, params).await,
            "session.export" => handlers::export::handle_export(runtime, token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "session pack does not handle verb {verb:?}; valid verbs: \
                 session.store, session.list, session.resume, session.export"
            ))),
        }
    }
}
