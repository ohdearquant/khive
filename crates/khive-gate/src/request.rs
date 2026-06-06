use khive_types::Namespace;
use serde::{Deserialize, Serialize};

use crate::{ActorRef, GateContext};

// ---------- Request ----------

/// What the gate sees on every verb invocation.
///
/// The JSON projection of this struct is the input shape policies receive
/// (e.g. Rego's `input.actor`, `input.verb`, `input.args`). The shape is a
/// public contract — changing field names is a breaking change.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GateRequest {
    pub actor: ActorRef,
    pub namespace: Namespace,
    pub verb: String,
    pub args: serde_json::Value,
    #[serde(default)]
    pub context: GateContext,
}

impl GateRequest {
    /// Builds a `GateRequest` with default (empty) context.
    pub fn new(
        actor: ActorRef,
        namespace: Namespace,
        verb: impl Into<String>,
        args: serde_json::Value,
    ) -> Self {
        Self {
            actor,
            namespace,
            verb: verb.into(),
            args,
            context: GateContext::default(),
        }
    }

    /// Attaches a `GateContext` (session, timestamp, source) to this request.
    pub fn with_context(mut self, context: GateContext) -> Self {
        self.context = context;
        self
    }
}
