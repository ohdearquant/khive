//! pack-comm — Communication pack (ADR-040).
pub mod handlers;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, Pack, Visibility};

pub struct CommPack {
    runtime: KhiveRuntime,
}

impl Pack for CommPack {
    const NAME: &'static str = "comm";
    const NOTE_KINDS: &'static [&'static str] = &["message"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &COMM_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

static COMM_HANDLERS: [HandlerDef; 4] = [
    HandlerDef {
        name: "send",
        description: "Send a message, optionally threaded.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Directive,
    },
    HandlerDef {
        name: "inbox",
        description: "List inbound messages for the caller.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Assertive,
    },
    HandlerDef {
        name: "read",
        description: "Mark a message as read.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Declaration,
    },
    HandlerDef {
        name: "reply",
        description: "Reply to a message, threading linkage.",
        visibility: Visibility::Verb,
        category: khive_types::VerbCategory::Directive,
    },
];

impl CommPack {
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }
    pub(crate) fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }
}

struct CommPackFactory;

impl khive_runtime::PackFactory for CommPackFactory {
    fn name(&self) -> &'static str {
        "comm"
    }
    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(CommPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&CommPackFactory) }

#[async_trait]
impl PackRuntime for CommPack {
    fn name(&self) -> &str {
        <CommPack as Pack>::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        <CommPack as Pack>::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        <CommPack as Pack>::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        &COMM_HANDLERS
    }
    fn requires(&self) -> &'static [&'static str] {
        <CommPack as Pack>::REQUIRES
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "send" => handlers::handle_send(self.runtime(), token, params).await,
            "inbox" => handlers::handle_inbox(self.runtime(), token, params).await,
            "read" => handlers::handle_read(self.runtime(), token, params).await,
            "reply" => handlers::handle_reply(self.runtime(), token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "comm pack does not handle verb {verb:?}"
            ))),
        }
    }
}
