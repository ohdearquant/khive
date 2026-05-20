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

static MEMORY_VERBS: [VerbDef; 2] = [
    VerbDef {
        name: "remember",
        description: "Create a memory note with salience and decay",
    },
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
