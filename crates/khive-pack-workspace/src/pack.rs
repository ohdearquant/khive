//! Workspace pack registration and runtime adapter.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, KindHook, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{EdgeEndpointRule, HandlerDef, Pack};

use crate::hook::WorkspaceHook;
use crate::vocab::{ENTITY_KINDS, WORKSPACE_EDGE_RULES};

/// Vocabulary-only workspace pack with five membership rules and no verbs.
///
/// See `crates/khive-pack-workspace/docs/api/workspace-registration.md`.
pub struct WorkspacePack {
    runtime: KhiveRuntime,
}

impl Pack for WorkspacePack {
    const NAME: &'static str = "workspace";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = ENTITY_KINDS;
    const HANDLERS: &'static [HandlerDef] = &[];
    const EDGE_RULES: &'static [EdgeEndpointRule] = &WORKSPACE_EDGE_RULES;
    const REQUIRES: &'static [&'static str] = &["kg", "gtd", "session"];
}

impl WorkspacePack {
    /// Bind the vocabulary-only workspace pack to `runtime`.
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }

    #[allow(dead_code)]
    pub(crate) fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }
}

struct WorkspacePackFactory;

impl khive_runtime::PackFactory for WorkspacePackFactory {
    fn name(&self) -> &'static str {
        "workspace"
    }

    fn requires(&self) -> &'static [&'static str] {
        &["kg", "gtd", "session"]
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(WorkspacePack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&WorkspacePackFactory) }

#[async_trait]
impl PackRuntime for WorkspacePack {
    fn name(&self) -> &str {
        <WorkspacePack as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <WorkspacePack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <WorkspacePack as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        <WorkspacePack as Pack>::HANDLERS
    }

    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        <WorkspacePack as Pack>::EDGE_RULES
    }

    fn requires(&self) -> &'static [&'static str] {
        <WorkspacePack as Pack>::REQUIRES
    }

    fn kind_hook(&self, kind: &str) -> Option<Arc<dyn KindHook>> {
        match kind {
            "workspace" => Some(Arc::new(WorkspaceHook)),
            _ => None,
        }
    }

    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "workspace pack does not handle verb {verb:?}; v0 exposes no verbs, use the \
             generic create/link KG verbs"
        )))
    }
}
