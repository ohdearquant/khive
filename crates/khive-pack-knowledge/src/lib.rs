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
use khive_types::{HandlerDef, Pack, ParamDef, VerbCategory, Visibility};

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
        name: "knowledge.learn",
        description: "Register a concept entity with optional domain and tags",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "name",
                param_type: "string",
                required: true,
                description: "Concept name",
            },
            ParamDef {
                name: "description",
                param_type: "string",
                required: false,
                description: "Optional concept description",
            },
            ParamDef {
                name: "domain",
                param_type: "string",
                required: false,
                description: "Optional domain tag (folded into properties.domain)",
            },
            ParamDef {
                name: "tags",
                param_type: "array<string>",
                required: false,
                description: "Optional tag list",
            },
        ],
    },
    // Commissive: commits an introduced_by edge between concept and source
    HandlerDef {
        name: "knowledge.cite",
        description: "Link a concept to the paper or source that introduced it",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[
            ParamDef {
                name: "concept_id",
                param_type: "uuid",
                required: true,
                description: "Concept entity ID",
            },
            ParamDef {
                name: "source_id",
                param_type: "uuid",
                required: true,
                description: "Source entity ID; must be kind=document or kind=person (ADR-002 §introduced_by)",
            },
            ParamDef {
                name: "weight",
                param_type: "float",
                required: false,
                description: "Edge weight; defaults to 1.0",
            },
        ],
    },
    // Assertive: retrieves concepts filtered by domain or free-text query
    HandlerDef {
        name: "knowledge.topic",
        description: "List concepts filtered by domain or free-text query",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "domain",
                param_type: "string",
                required: false,
                description: "Filter to concepts with this domain tag",
            },
            ParamDef {
                name: "query",
                param_type: "string",
                required: false,
                description: "Free-text search across concept name + description",
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Max results; defaults to 20, capped at 100",
            },
        ],
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
            "knowledge.learn" => self.handle_learn(token, params).await,
            "knowledge.cite" => self.handle_cite(token, params).await,
            "knowledge.topic" => self.handle_topic(token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "knowledge pack does not handle verb {verb:?}"
            ))),
        }
    }
}
