//! Agent verb pack — spawn/resume/kill/suspend/observe wire surface.
//!
//! ADR-142 §1: "An agent is a runtime-owned process record and is not a
//! pack... A dedicated agent pack registers the verbs `agent.spawn`,
//! `agent.resume`, `agent.kill`, `agent.suspend`, and `agent.observe` with
//! the verb registry... The pack owns the wire surface; the runtime owns
//! the table." This crate is that wire surface: it validates parameters,
//! computes the spawn fingerprint, and drives the lifecycle transition
//! table, entirely through the `AgentStore` trait — it never opens a
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

/// Agent pack: the ADR-142 wire surface over the runtime-owned agent table.
///
/// `KhiveRuntime` has no accessor for the agent table to reach into — that
/// accessor is not part of the shared contract this pack was built against
/// — so `AgentStore` is supplied directly at construction: the same shape
/// `BlobPack` uses for `BlobStore`, except sourced from the caller rather
/// than resolved from `KhiveRuntime` config. Handlers never see anything
/// but the trait object, and this pack holds no `KhiveRuntime` handle of
/// its own — every ADR-142 §1 operation this pack performs goes through
/// `AgentStore` alone.
pub struct AgentPack {
    store: Arc<dyn AgentStore>,
}

impl Pack for AgentPack {
    const NAME: &'static str = PACK_NAME;
    const NOTE_KINDS: &'static [&'static str] = vocab::NOTE_KINDS;
    const ENTITY_KINDS: &'static [&'static str] = vocab::ENTITY_KINDS;
    const HANDLERS: &'static [HandlerDef] = &AGENT_HANDLERS;
    const REQUIRES: &'static [&'static str] = &[];
}

impl AgentPack {
    /// Bind the agent pack to the runtime-owned agent store.
    pub fn new(store: Arc<dyn AgentStore>) -> Self {
        Self { store }
    }

    pub(crate) fn store(&self) -> &Arc<dyn AgentStore> {
        &self.store
    }
}
