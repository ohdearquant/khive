//! pack-kg — Knowledge Graph verb pack for khive.
//!
//! Provides 11 verbs for managing entities, notes, edges, and graph queries
//! in a research knowledge graph. This is the first-party pack shipped with
//! the khive binary.

pub mod handlers;
pub mod vocab;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, Pack, Visibility};

pub use khive_types::EntityKind;
pub use vocab::NoteKind;

/// KG pack vocabulary declaration.
pub struct KgPack {
    runtime: KhiveRuntime,
}

impl Pack for KgPack {
    const NAME: &'static str = "kg";
    const NOTE_KINDS: &'static [&'static str] = &[
        "observation",
        "insight",
        "question",
        "decision",
        "reference",
    ];
    const ENTITY_KINDS: &'static [&'static str] = &[
        "concept", "document", "dataset", "project", "person", "org", "artifact", "service",
    ];
    const HANDLERS: &'static [HandlerDef] = &KG_HANDLERS;
}

// ADR-060: Illocutionary classification (Searle 1976)
//   Assertive — retrieves/presents state of affairs
//   Commissive — commits caller to a persistent change
//   Declaration — changes institutional status by fiat
static KG_HANDLERS: [HandlerDef; 11] = [
    // Commissive: commits an entity or note to the namespace
    HandlerDef {
        name: "create",
        description: "Create an entity or note",
        visibility: Visibility::Verb,
    },
    // Assertive: retrieves and presents a record
    HandlerDef {
        name: "get",
        description: "Fetch any record by UUID",
        visibility: Visibility::Verb,
    },
    // Assertive: retrieves and presents filtered records
    HandlerDef {
        name: "list",
        description: "List records with optional filtering",
        visibility: Visibility::Verb,
    },
    // Declaration: changes entity or edge state by fiat
    HandlerDef {
        name: "update",
        description: "Patch entity or edge fields",
        visibility: Visibility::Verb,
    },
    // Declaration: declares a record removed
    HandlerDef {
        name: "delete",
        description: "Soft or hard delete a record",
        visibility: Visibility::Verb,
    },
    // Declaration: declares two entities identical
    HandlerDef {
        name: "merge",
        description: "Deduplicate two entities",
        visibility: Visibility::Verb,
    },
    // Assertive: retrieves and presents search results
    HandlerDef {
        name: "search",
        description: "Hybrid FTS + vector search",
        visibility: Visibility::Verb,
    },
    // Commissive: commits a typed edge to the graph
    HandlerDef {
        name: "link",
        description: "Create a typed directed edge",
        visibility: Visibility::Verb,
    },
    // Assertive: retrieves immediate graph neighbors
    HandlerDef {
        name: "neighbors",
        description: "Immediate graph neighbors",
        visibility: Visibility::Verb,
    },
    // Assertive: retrieves multi-hop traversal results
    HandlerDef {
        name: "traverse",
        description: "Multi-hop BFS traversal",
        visibility: Visibility::Verb,
    },
    // Assertive: retrieves pattern-matched results
    HandlerDef {
        name: "query",
        description: "GQL/SPARQL pattern matching",
        visibility: Visibility::Verb,
    },
];

impl KgPack {
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }
}

// ── ADR-027: inventory self-registration ─────────────────────────────────────

struct KgPackFactory;

impl khive_runtime::PackFactory for KgPackFactory {
    fn name(&self) -> &'static str {
        "kg"
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(KgPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&KgPackFactory) }

#[async_trait]
impl PackRuntime for KgPack {
    fn name(&self) -> &str {
        "kg"
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <KgPack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <KgPack as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        &KG_HANDLERS
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "create" => self.handle_create(token, params, registry).await,
            "get" => self.handle_get(token, params).await,
            "list" => self.handle_list(token, params, registry).await,
            "update" => self.handle_update(token, params, registry).await,
            "delete" => self.handle_delete(token, params, registry).await,
            "merge" => self.handle_merge(token, params, registry).await,
            "search" => self.handle_search(token, params, registry).await,
            "link" => self.handle_link(token, params).await,
            "neighbors" => self.handle_neighbors(token, params).await,
            "traverse" => self.handle_traverse(token, params).await,
            "query" => self.handle_query(token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "kg pack does not handle verb {verb:?}"
            ))),
        }
    }
}
