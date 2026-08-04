//! Handler table and runtime dispatch for the agent pack.
//!
//! Unlike the other packs in this workspace, `AgentPack` is not registered
//! through `inventory::submit!`/`PackFactory`: that self-registration path
//! constructs a pack from a bare `KhiveRuntime` alone, and `KhiveRuntime`
//! has no accessor for an `AgentStore` (no such accessor is part of the
//! shared contract this crate was built against, and adding one is outside
//! this crate's file scope). Callers construct `AgentPack::new(runtime,
//! store)` directly and register the instance with
//! `RegistryBuilder::register`, the same manual path already supported for
//! any `Pack + PackRuntime` value.

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, ParamDef, Visibility};

use crate::{handlers, AgentPack, PACK_NAME};

pub(crate) static AGENT_HANDLERS: [HandlerDef; 5] = [
    HandlerDef {
        name: "agent.spawn",
        description: "Create a new agent process record and return its id and initial state \
                       (ADR-142 §1).",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "provider",
                param_type: "string",
                required: true,
                description: "Name of the model provider adapter to run this agent under.",
            },
            ParamDef {
                name: "task",
                param_type: "string",
                required: true,
                description: "Initial instruction content for the spawned agent.",
            },
            ParamDef {
                name: "idempotency_key",
                param_type: "string",
                required: false,
                description: "Caller-supplied replay key, scoped to the calling actor; a \
                               repeat with identical arguments returns the original record.",
            },
            ParamDef {
                name: "provider_session_id",
                param_type: "string",
                required: false,
                description: "Provider-native continuity key; at most one non-terminal record \
                               may bind a given (provider, provider_session_id) pair.",
            },
            ParamDef {
                name: "checkpoint_session_id",
                param_type: "string",
                required: false,
                description: "Khive session-note identifier of a checkpoint to continue from.",
            },
        ],
    },
    HandlerDef {
        name: "agent.observe",
        description: "Report an agent process record's current fields without changing state \
                       (ADR-142 §1).",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
        params: &[ParamDef {
            name: "id",
            param_type: "string",
            required: true,
            description: "The agent_id to observe.",
        }],
    },
    HandlerDef {
        name: "agent.suspend",
        description: "Transition a running agent process to suspended at a message-yield \
                       boundary (ADR-142 §1).",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Directive,
        params: &[ParamDef {
            name: "id",
            param_type: "string",
            required: true,
            description: "The agent_id to suspend.",
        }],
    },
    HandlerDef {
        name: "agent.resume",
        description: "Transition a suspended agent process back to running (ADR-142 §1).",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Directive,
        params: &[ParamDef {
            name: "id",
            param_type: "string",
            required: true,
            description: "The agent_id to resume.",
        }],
    },
    HandlerDef {
        name: "agent.kill",
        description: "Transition an agent process to terminal/killed; a no-op returning the \
                       current state when already terminal (ADR-142 §1).",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Directive,
        params: &[ParamDef {
            name: "id",
            param_type: "string",
            required: true,
            description: "The agent_id to kill.",
        }],
    },
];

#[async_trait]
impl PackRuntime for AgentPack {
    fn name(&self) -> &str {
        PACK_NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        crate::vocab::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        crate::vocab::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        &AGENT_HANDLERS
    }

    fn requires(&self) -> &'static [&'static str] {
        &[]
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "agent.spawn" => handlers::handle_spawn(self.store(), token, params).await,
            "agent.observe" => handlers::handle_observe(self.store(), params).await,
            "agent.suspend" => handlers::handle_suspend(self.store(), params).await,
            "agent.resume" => handlers::handle_resume(self.store(), params).await,
            "agent.kill" => handlers::handle_kill(self.store(), params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "{PACK_NAME} pack does not handle verb {verb:?}"
            ))),
        }
    }
}
