//! Pack runtime trait and verb registry (ADR-025 step 2).
//!
//! Packs register verbs into the runtime. The registry routes verb calls
//! to the pack that declares them.

use async_trait::async_trait;
use serde_json::Value;

pub use khive_types::VerbDef;

use crate::error::RuntimeError;

/// Async dispatch trait for packs (ADR-025).
///
/// This is the behavioral counterpart to `khive_types::Pack`. The `Pack` trait
/// provides static metadata via const items (not object-safe); this trait
/// mirrors that metadata as methods and adds async dispatch.
///
/// Implementors must also implement `Pack` on the same struct — the `name()`
/// and `verbs()` methods here should return the same values as `Pack::NAME`
/// and `Pack::VERBS`.
#[async_trait]
pub trait PackRuntime: Send + Sync {
    /// Pack name — must match `Pack::NAME` on the implementing struct.
    fn name(&self) -> &str;

    /// Note kinds this pack owns — must match `Pack::NOTE_KINDS`.
    fn note_kinds(&self) -> &'static [&'static str];

    /// Entity kinds this pack owns — must match `Pack::ENTITY_KINDS`.
    fn entity_kinds(&self) -> &'static [&'static str];

    /// Verbs this pack handles — must match `Pack::VERBS`.
    fn verbs(&self) -> &'static [VerbDef];

    /// Dispatch a verb call. Returns serialized JSON response.
    async fn dispatch(&self, verb: &str, params: Value) -> Result<Value, RuntimeError>;
}

/// Registry that collects packs and dispatches verb calls.
///
/// Clone is cheap (Arc-wrapped internally).
#[derive(Clone)]
pub struct VerbRegistry {
    packs: std::sync::Arc<Vec<Box<dyn PackRuntime>>>,
}

impl VerbRegistry {
    pub fn new() -> Self {
        Self {
            packs: std::sync::Arc::new(Vec::new()),
        }
    }

    pub fn register(&mut self, pack: impl PackRuntime + 'static) {
        std::sync::Arc::get_mut(&mut self.packs)
            .expect("register must be called before cloning")
            .push(Box::new(pack));
    }

    /// Dispatch a verb to the first pack that handles it.
    pub async fn dispatch(&self, verb: &str, params: Value) -> Result<Value, RuntimeError> {
        for pack in self.packs.iter() {
            if pack.verbs().iter().any(|v| v.name == verb) {
                return pack.dispatch(verb, params).await;
            }
        }
        let available: Vec<&str> = self
            .packs
            .iter()
            .flat_map(|p| p.verbs().iter().map(|v| v.name))
            .collect();
        Err(RuntimeError::InvalidInput(format!(
            "unknown verb {verb:?}; available: {}",
            available.join(", ")
        )))
    }

    /// All verb definitions across all registered packs.
    pub fn all_verbs(&self) -> Vec<&VerbDef> {
        self.packs.iter().flat_map(|p| p.verbs().iter()).collect()
    }

    /// All note kinds across all registered packs (merged set).
    pub fn all_note_kinds(&self) -> Vec<&'static str> {
        self.packs
            .iter()
            .flat_map(|p| p.note_kinds().iter().copied())
            .collect()
    }

    /// All entity kinds across all registered packs (merged set).
    pub fn all_entity_kinds(&self) -> Vec<&'static str> {
        self.packs
            .iter()
            .flat_map(|p| p.entity_kinds().iter().copied())
            .collect()
    }
}

impl Default for VerbRegistry {
    fn default() -> Self {
        Self::new()
    }
}
