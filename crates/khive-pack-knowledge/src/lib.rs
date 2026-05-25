//! pack-knowledge — structured knowledge management verbs for khive.
//!
//! Provides three higher-level verbs that build on the kg substrate:
//!
//! - `learn`  — register a concept entity (commissive).
//! - `cite`   — link a concept to its source paper via `introduced_by` (commissive).
//! - `topic`  — list/search concepts, optionally filtered by domain (assertive).
//!
//! Load with `KHIVE_PACKS=kg,knowledge` or `--pack knowledge`.

pub mod handlers;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, Pack, VerbCategory, Visibility};

pub struct KnowledgePack {
    pub(crate) runtime: KhiveRuntime,
}

impl Pack for KnowledgePack {
    const NAME: &'static str = "knowledge";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &KNOWLEDGE_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

// ADR-025: Illocutionary classification
//   Commissive — commits caller to a persistent change
//   Assertive  — retrieves/presents state of affairs
static KNOWLEDGE_HANDLERS: [HandlerDef; 3] = [
    // Commissive: commits a concept entity to the namespace
    HandlerDef {
        name: "learn",
        description: "Register a concept entity with optional domain and tags",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
    },
    // Commissive: commits an introduced_by edge between concept and source
    HandlerDef {
        name: "cite",
        description: "Link a concept to the paper or source that introduced it",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
    },
    // Assertive: retrieves concepts filtered by domain or free-text query
    HandlerDef {
        name: "topic",
        description: "List concepts filtered by domain or free-text query",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
    },
];

impl KnowledgePack {
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }
}

// ── ADR-027: inventory self-registration ─────────────────────────────────────

struct KnowledgePackFactory;

impl khive_runtime::PackFactory for KnowledgePackFactory {
    fn name(&self) -> &'static str {
        "knowledge"
    }

    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(KnowledgePack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&KnowledgePackFactory) }

#[async_trait]
impl PackRuntime for KnowledgePack {
    fn name(&self) -> &str {
        <KnowledgePack as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <KnowledgePack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <KnowledgePack as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        &KNOWLEDGE_HANDLERS
    }

    fn requires(&self) -> &'static [&'static str] {
        <KnowledgePack as Pack>::REQUIRES
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "learn" => self.handle_learn(token, params).await,
            "cite" => self.handle_cite(token, params).await,
            "topic" => self.handle_topic(token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "knowledge pack does not handle verb {verb:?}"
            ))),
        }
    }
}
