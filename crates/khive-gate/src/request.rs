use khive_types::Namespace;
use serde::{Deserialize, Serialize};

use crate::{ActorRef, GateContext, GateValidationError};

/// What the gate sees on every verb invocation.
///
/// Its JSON fields are a stable policy-input contract; `verb` must be non-empty. See
/// `crates/khive-gate/docs/api/policy-types.md`.
#[derive(Clone, Debug, Serialize)]
pub struct GateRequest {
    pub actor: ActorRef,
    pub namespace: Namespace,
    pub verb: String,
    pub args: serde_json::Value,
    #[serde(default)]
    pub context: GateContext,
}

/// Raw deserialization target for [`GateRequest`] — validated via `TryFrom`.
#[derive(Deserialize)]
struct RawGateRequest {
    actor: ActorRef,
    namespace: Namespace,
    verb: String,
    args: serde_json::Value,
    #[serde(default)]
    context: GateContext,
}

impl TryFrom<RawGateRequest> for GateRequest {
    type Error = GateValidationError;

    fn try_from(raw: RawGateRequest) -> Result<Self, Self::Error> {
        if raw.verb.is_empty() {
            return Err(GateValidationError::EmptyVerb);
        }
        Ok(Self {
            actor: raw.actor,
            namespace: raw.namespace,
            verb: raw.verb,
            args: raw.args,
            context: raw.context,
        })
    }
}

impl<'de> Deserialize<'de> for GateRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawGateRequest::deserialize(deserializer)?;
        GateRequest::try_from(raw).map_err(serde::de::Error::custom)
    }
}

impl GateRequest {
    /// Create a validated `GateRequest`. Returns `Err` if `verb` is empty.
    pub fn try_new(
        actor: ActorRef,
        namespace: Namespace,
        verb: impl Into<String>,
        args: serde_json::Value,
    ) -> Result<Self, GateValidationError> {
        let verb = verb.into();
        if verb.is_empty() {
            return Err(GateValidationError::EmptyVerb);
        }
        Ok(Self {
            actor,
            namespace,
            verb,
            args,
            context: GateContext::default(),
        })
    }

    /// Builds a `GateRequest` with default (empty) context. Panics if `verb` is empty.
    pub fn new(
        actor: ActorRef,
        namespace: Namespace,
        verb: impl Into<String>,
        args: serde_json::Value,
    ) -> Self {
        Self::try_new(actor, namespace, verb, args)
            .expect("GateRequest::new: verb must not be empty")
    }

    /// Attaches a `GateContext` (session, timestamp, source) to this request.
    pub fn with_context(mut self, context: GateContext) -> Self {
        self.context = context;
        self
    }
}
