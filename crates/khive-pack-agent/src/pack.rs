//! Handler table and runtime dispatch for the opt-in agent pack.

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, IdResolutionMode, ParamDef, Visibility};

use crate::{handlers, AgentPack, PACK_NAME};

pub(crate) static AGENT_HANDLERS: [HandlerDef; 5] = [
    HandlerDef {
        name: "agent.spawn",
        description:
            "Request an agent process; refuses provider_unavailable until an adapter exists.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "provider",
                param_type: "string",
                required: true,
                description: "Name of the model provider adapter to run this agent under.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "task",
                param_type: "string",
                required: true,
                description: "Initial instruction content for the spawned agent.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "idempotency_key",
                param_type: "string",
                required: false,
                description: "Reserved replay key; spawn currently refuses provider_unavailable.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "provider_session_id",
                param_type: "string",
                required: false,
                description: "Provider-native continuity key; at most one non-terminal record \
                               may bind a given (provider, provider_session_id) pair.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "checkpoint_session_id",
                param_type: "string",
                required: false,
                description: "Khive session-note identifier of a checkpoint to continue from.",
                resolution_mode: IdResolutionMode::NotApplicable,
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
            resolution_mode: IdResolutionMode::NotApplicable,
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
            resolution_mode: IdResolutionMode::NotApplicable,
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
            resolution_mode: IdResolutionMode::NotApplicable,
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
            resolution_mode: IdResolutionMode::NotApplicable,
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
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "agent.spawn" => handlers::handle_spawn(params),
            "agent.observe" => handlers::handle_observe(self.store().await?, params).await,
            "agent.suspend" => handlers::handle_suspend(self.store().await?, params).await,
            "agent.resume" => handlers::handle_resume(self.store().await?, params).await,
            "agent.kill" => handlers::handle_kill(self.store().await?, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "{PACK_NAME} pack does not handle verb {verb:?}"
            ))),
        }
    }
}

struct AgentPackFactory;
impl khive_runtime::PackFactory for AgentPackFactory {
    fn name(&self) -> &'static str {
        PACK_NAME
    }
    fn requires(&self) -> &'static [&'static str] {
        &[]
    }
    fn create(&self, runtime: khive_runtime::KhiveRuntime) -> Box<dyn PackRuntime> {
        Box::new(AgentPack::from_runtime(runtime))
    }
}
inventory::submit! { khive_runtime::PackRegistration(&AgentPackFactory) }
