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
use khive_runtime::{KhiveRuntime, RuntimeError, VerbRegistry};
use khive_types::{Pack, VerbDef};

pub use vocab::{EntityKind, NoteKind};

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
    const ENTITY_KINDS: &'static [&'static str] =
        &["concept", "document", "dataset", "project", "person", "org"];
    const VERBS: &'static [VerbDef] = &KG_VERBS;
}

static KG_VERBS: [VerbDef; 11] = [
    VerbDef {
        name: "create",
        description: "Create an entity or note",
    },
    VerbDef {
        name: "get",
        description: "Fetch any record by UUID",
    },
    VerbDef {
        name: "list",
        description: "List records with optional filtering",
    },
    VerbDef {
        name: "update",
        description: "Patch entity or edge fields",
    },
    VerbDef {
        name: "delete",
        description: "Soft or hard delete a record",
    },
    VerbDef {
        name: "merge",
        description: "Deduplicate two entities",
    },
    VerbDef {
        name: "search",
        description: "Hybrid FTS + vector search",
    },
    VerbDef {
        name: "link",
        description: "Create a typed directed edge",
    },
    VerbDef {
        name: "neighbors",
        description: "Immediate graph neighbors",
    },
    VerbDef {
        name: "traverse",
        description: "Multi-hop BFS traversal",
    },
    VerbDef {
        name: "query",
        description: "GQL/SPARQL pattern matching",
    },
];

impl KgPack {
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }
}

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

    fn verbs(&self) -> &'static [VerbDef] {
        &KG_VERBS
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "create" => self.handle_create(params, registry).await,
            "get" => self.handle_get(params).await,
            "list" => self.handle_list(params, registry).await,
            "update" => self.handle_update(params).await,
            "delete" => self.handle_delete(params).await,
            "merge" => self.handle_merge(params).await,
            "search" => self.handle_search(params, registry).await,
            "link" => self.handle_link(params).await,
            "neighbors" => self.handle_neighbors(params).await,
            "traverse" => self.handle_traverse(params).await,
            "query" => self.handle_query(params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "kg pack does not handle verb {verb:?}"
            ))),
        }
    }
}
