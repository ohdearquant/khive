//! `ExecPack` self-registration factory and `PackRuntime` dispatch impl.

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, Pack, PackSchemaPlan};

use crate::handlers;
use crate::sandbox::{self, Resolved};
use crate::vocab::{EXEC_HANDLERS, EXEC_SCHEMA_PLAN_STMTS, PACK_NAME};

pub struct ExecPack {
    runtime: KhiveRuntime,
    config: Resolved,
}

impl ExecPack {
    /// Build from the runtime's resolved `[exec]` section.
    pub fn new(runtime: KhiveRuntime) -> Self {
        let config = sandbox::resolve(&runtime.config().exec);
        Self { runtime, config }
    }

    pub(crate) fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }

    pub fn config(&self) -> &Resolved {
        &self.config
    }
}

impl Pack for ExecPack {
    const NAME: &'static str = PACK_NAME;
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &EXEC_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["blob", "tool"];
    const SCHEMA_PLAN: Option<PackSchemaPlan> = Some(PackSchemaPlan {
        pack: PACK_NAME,
        statements: &EXEC_SCHEMA_PLAN_STMTS,
    });
}

struct ExecPackFactory;

impl khive_runtime::PackFactory for ExecPackFactory {
    khive_runtime::pack_factory_metadata!(ExecPack);

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(ExecPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&ExecPackFactory) }

#[async_trait]
impl PackRuntime for ExecPack {
    khive_runtime::pack_runtime_metadata!();

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        let rt = self.runtime();
        match verb {
            "exec.tree" => handlers::tree_store(rt, params).await,
            "exec.tree_get" => handlers::tree_get(rt, params).await,
            "exec.tree_put" => handlers::tree_put(rt, params).await,
            "exec.tree_diff" => handlers::tree_diff(rt, params).await,
            "exec.run" => handlers::run(rt, token, &self.config, params).await,
            "exec.receipt" => handlers::receipt(rt, token, params).await,
            "exec.runs" => handlers::runs(rt, token, params).await,
            "exec.events" => handlers::events(rt, token, params).await,
            "exec.identity" => Ok(handlers::identity(&self.config)),
            _ => Err(RuntimeError::InvalidInput(format!(
                "{PACK_NAME} pack does not handle verb {verb:?}"
            ))),
        }
    }
}
