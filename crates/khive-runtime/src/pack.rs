//! Pack runtime trait and verb registry.
//!
//! Packs register verbs into the runtime. The MCP server exposes a single
//! `request` tool that dispatches to the appropriate pack handler.

use async_trait::async_trait;
use serde_json::Value;

pub use khive_types::VerbDef;

use crate::error::RuntimeError;

/// Async trait for packs that handle verb dispatch.
///
/// Each pack owns a clone of KhiveRuntime (cheap — Arc internally) and
/// handles a set of verbs. The registry routes verb calls to the correct pack.
#[async_trait]
pub trait PackRuntime: Send + Sync {
    /// Pack name (matches Pack::NAME from khive-types).
    fn name(&self) -> &str;

    /// Verbs this pack handles.
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
}

impl Default for VerbRegistry {
    fn default() -> Self {
        Self::new()
    }
}
