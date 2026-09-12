//! Agent verb pack — spawn/resume/kill/suspend/observe wire surface.
//!
//! ADR-142 §1: "An agent is a runtime-owned process record and is not a
//! pack... A dedicated agent pack registers the verbs `agent.spawn`,
//! `agent.resume`, `agent.kill`, `agent.suspend`, and `agent.observe` with
//! the verb registry... The pack owns the wire surface; the runtime owns
//! the table." This crate is that wire surface: it validates parameters,
//! refuses unavailable providers, and drives the stored-record lifecycle
//! table through the `AgentStore` trait — it never opens a
//! khive-db connection of its own.

pub mod handlers;
mod pack;
pub mod vocab;

use std::sync::Arc;

use khive_storage::AgentStore;
use khive_types::{HandlerDef, Pack};

pub(crate) use pack::AGENT_HANDLERS;

/// Canonical pack name. Verbs are exposed as `agent.<verb>`.
pub(crate) const PACK_NAME: &str = "agent";

/// Agent verbs over a supplied store or the selected runtime's agent store.
pub struct AgentPack {
    store: tokio::sync::OnceCell<Arc<dyn AgentStore>>,
    runtime: Option<khive_runtime::KhiveRuntime>,
}

impl Pack for AgentPack {
    const NAME: &'static str = PACK_NAME;
    const NOTE_KINDS: &'static [&'static str] = vocab::NOTE_KINDS;
    const ENTITY_KINDS: &'static [&'static str] = vocab::ENTITY_KINDS;
    const HANDLERS: &'static [HandlerDef] = &AGENT_HANDLERS;
    const REQUIRES: &'static [&'static str] = &[];
}

impl AgentPack {
    /// Bind the agent pack to an existing agent store.
    pub fn new(store: Arc<dyn AgentStore>) -> Self {
        Self {
            store: tokio::sync::OnceCell::new_with(Some(store)),
            runtime: None,
        }
    }

    /// Resolve the selected runtime's store lazily, off the async executor.
    pub fn from_runtime(runtime: khive_runtime::KhiveRuntime) -> Self {
        Self {
            store: tokio::sync::OnceCell::new(),
            runtime: Some(runtime),
        }
    }

    pub(crate) async fn store(&self) -> Result<&Arc<dyn AgentStore>, khive_runtime::RuntimeError> {
        self.store
            .get_or_try_init(|| async {
                let runtime = self.runtime.clone().ok_or_else(|| {
                    khive_runtime::RuntimeError::Internal("agent store unavailable".into())
                })?;
                tokio::task::spawn_blocking(move || runtime.backend().agents())
                    .await
                    .map_err(|_| {
                        khive_runtime::RuntimeError::Internal(
                            "agent store initialization failed".into(),
                        )
                    })?
                    .map_err(khive_runtime::RuntimeError::from)
            })
            .await
    }
}
