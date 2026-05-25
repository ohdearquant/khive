pub mod config;
pub mod handlers;
pub mod tunable;

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, Pack, VerbCategory, Visibility};

use crate::config::RecallConfig;

pub struct MemoryPack {
    runtime: KhiveRuntime,
    /// Active recall config.
    config: Mutex<RecallConfig>,
}

impl MemoryPack {
    /// Return a clone of the current active `RecallConfig`.
    ///
    /// Handlers call this to pick up the latest tuned parameters.
    pub(crate) fn active_config(&self) -> RecallConfig {
        self.config.lock().unwrap().clone()
    }
}

impl Pack for MemoryPack {
    const NAME: &'static str = "memory";
    const NOTE_KINDS: &'static [&'static str] = &["memory"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &MEMORY_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

// ADR-025: Illocutionary classification (Searle 1976)
//   Commissive — commits caller to a persistent change
//   Assertive  — retrieves/presents state of affairs
static MEMORY_HANDLERS: [HandlerDef; 7] = [
    // Commissive: commits a memory to the namespace
    HandlerDef {
        name: "remember",
        description: "Create a memory note with salience and decay",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
    },
    // Assertive: retrieves memory notes via decay-aware ranking
    HandlerDef {
        name: "recall",
        description: "Recall memory notes with decay-aware hybrid ranking",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
    },
    HandlerDef {
        name: "recall.embed",
        description: "Return the embedding vector used by memory recall",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Assertive,
    },
    HandlerDef {
        name: "recall.candidates",
        description: "Return raw memory recall candidates by retrieval source",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Assertive,
    },
    HandlerDef {
        name: "recall.fuse",
        description: "Return fused memory recall candidates before final scoring",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Assertive,
    },
    // ADR-033 §2, F222: rerank stage between fuse and score
    HandlerDef {
        name: "recall.rerank",
        description: "Apply configured rerankers to fused candidates (ADR-033 §2)",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Assertive,
    },
    HandlerDef {
        name: "recall.score",
        description: "Score a memory recall candidate and return score breakdown",
        visibility: Visibility::Subhandler,
        category: VerbCategory::Assertive,
    },
];

impl MemoryPack {
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self {
            runtime,
            config: Mutex::new(RecallConfig::default()),
        }
    }
}

// ── ADR-027: inventory self-registration ─────────────────────────────────────

struct MemoryPackFactory;

impl khive_runtime::PackFactory for MemoryPackFactory {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(MemoryPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&MemoryPackFactory) }

#[async_trait]
impl PackRuntime for MemoryPack {
    fn name(&self) -> &str {
        <MemoryPack as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <MemoryPack as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <MemoryPack as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        &MEMORY_HANDLERS
    }

    fn requires(&self) -> &'static [&'static str] {
        <MemoryPack as Pack>::REQUIRES
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "remember" => self.handle_remember(token, params).await,
            "recall" => self.handle_recall(token, params, registry).await,
            "recall.embed" => self.handle_recall_embed(params).await,
            "recall.candidates" => self.handle_recall_candidates(token, params).await,
            "recall.fuse" => self.handle_recall_fuse(token, params, registry).await,
            "recall.rerank" => self.handle_recall_rerank(params).await,
            "recall.score" => self.handle_recall_score(params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "memory pack does not handle verb {verb:?}"
            ))),
        }
    }
}
