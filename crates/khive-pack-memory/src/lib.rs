pub mod handlers;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, RuntimeError, VerbRegistry};
use khive_types::{Pack, VerbDef};

pub struct MemoryPack {
    runtime: KhiveRuntime,
}

impl Pack for MemoryPack {
    const NAME: &'static str = "memory";
    const NOTE_KINDS: &'static [&'static str] = &["memory"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const VERBS: &'static [VerbDef] = &MEMORY_VERBS;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

// ADR-060: Illocutionary classification (Searle 1976)
//   Commissive — commits caller to a persistent change
//   Assertive — retrieves/presents state of affairs
static MEMORY_VERBS: [VerbDef; 2] = [
    // Commissive: commits a memory to the namespace
    VerbDef {
        name: "remember",
        description: "Create a memory note with salience and decay",
    },
    // Assertive: retrieves memory notes via decay-aware ranking
    VerbDef {
        name: "recall",
        description: "Recall memory notes with decay-aware hybrid ranking",
    },
];

impl MemoryPack {
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }
}

// ── ADR-063: inventory self-registration ─────────────────────────────────────

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

    fn verbs(&self) -> &'static [VerbDef] {
        &MEMORY_VERBS
    }

    fn requires(&self) -> &'static [&'static str] {
        <MemoryPack as Pack>::REQUIRES
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "remember" => self.handle_remember(params).await,
            "recall" => self.handle_recall(params, registry).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "memory pack does not handle verb {verb:?}"
            ))),
        }
    }
}
