//! Pack runtime trait and verb registry (ADR-025 step 2).
//!
//! Packs register verbs into the runtime. The registry routes verb calls
//! to the pack that declares them.
//!
//! `Pack` (in khive-types) uses const associated items which are not
//! object-safe. `PackRuntime` mirrors that metadata as methods so the
//! registry can store packs as `Box<dyn PackRuntime>`. See ADR-025
//! §PackRuntime for the rationale.

use async_trait::async_trait;
use serde_json::Value;

pub use khive_types::VerbDef;

use crate::error::RuntimeError;

/// Async dispatch trait for packs (ADR-025).
///
/// This is the object-safe behavioral counterpart to `khive_types::Pack`.
/// `Pack` uses const associated items (not object-safe in Rust); this trait
/// mirrors that metadata as methods and adds async dispatch.
///
/// Implementors must also implement `Pack` on the same struct. The methods
/// here must return the same values as the corresponding `Pack` consts.
#[async_trait]
pub trait PackRuntime: Send + Sync {
    /// Pack name — must equal `<Self as Pack>::NAME`.
    fn name(&self) -> &str;

    /// Note kinds this pack owns — must equal `<Self as Pack>::NOTE_KINDS`.
    fn note_kinds(&self) -> &'static [&'static str];

    /// Entity kinds this pack owns — must equal `<Self as Pack>::ENTITY_KINDS`.
    fn entity_kinds(&self) -> &'static [&'static str];

    /// Verbs this pack handles — must equal `<Self as Pack>::VERBS`.
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

    /// Merged set of note kinds across all registered packs (deduplicated,
    /// first-seen order preserved).
    pub fn all_note_kinds(&self) -> Vec<&'static str> {
        let mut seen = std::collections::HashSet::new();
        self.packs
            .iter()
            .flat_map(|p| p.note_kinds().iter().copied())
            .filter(|k| seen.insert(*k))
            .collect()
    }

    /// Merged set of entity kinds across all registered packs (deduplicated,
    /// first-seen order preserved).
    pub fn all_entity_kinds(&self) -> Vec<&'static str> {
        let mut seen = std::collections::HashSet::new();
        self.packs
            .iter()
            .flat_map(|p| p.entity_kinds().iter().copied())
            .filter(|k| seen.insert(*k))
            .collect()
    }
}

impl Default for VerbRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlphaPack;

    #[async_trait]
    impl PackRuntime for AlphaPack {
        fn name(&self) -> &str {
            "alpha"
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            &["memo", "log"]
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            &["widget"]
        }
        fn verbs(&self) -> &'static [VerbDef] {
            static VERBS: [VerbDef; 2] = [
                VerbDef {
                    name: "create",
                    description: "create a widget",
                },
                VerbDef {
                    name: "list",
                    description: "list widgets",
                },
            ];
            &VERBS
        }
        async fn dispatch(&self, verb: &str, _params: Value) -> Result<Value, RuntimeError> {
            Ok(serde_json::json!({ "pack": "alpha", "verb": verb }))
        }
    }

    struct BetaPack;

    #[async_trait]
    impl PackRuntime for BetaPack {
        fn name(&self) -> &str {
            "beta"
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            &["log", "alert"]
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            &["widget", "gadget"]
        }
        fn verbs(&self) -> &'static [VerbDef] {
            static VERBS: [VerbDef; 1] = [VerbDef {
                name: "notify",
                description: "send alert",
            }];
            &VERBS
        }
        async fn dispatch(&self, verb: &str, _params: Value) -> Result<Value, RuntimeError> {
            Ok(serde_json::json!({ "pack": "beta", "verb": verb }))
        }
    }

    #[tokio::test]
    async fn dispatch_routes_to_correct_pack() {
        let mut reg = VerbRegistry::new();
        reg.register(AlphaPack);
        reg.register(BetaPack);

        let res = reg.dispatch("create", Value::Null).await.unwrap();
        assert_eq!(res["pack"], "alpha");

        let res = reg.dispatch("notify", Value::Null).await.unwrap();
        assert_eq!(res["pack"], "beta");
    }

    #[tokio::test]
    async fn dispatch_unknown_verb_returns_error() {
        let mut reg = VerbRegistry::new();
        reg.register(AlphaPack);

        let err = reg.dispatch("explode", Value::Null).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("explode"));
        assert!(msg.contains("create"));
        assert!(msg.contains("list"));
    }

    #[test]
    fn all_verbs_aggregates_across_packs() {
        let mut reg = VerbRegistry::new();
        reg.register(AlphaPack);
        reg.register(BetaPack);

        let verbs: Vec<&str> = reg.all_verbs().iter().map(|v| v.name).collect();
        assert_eq!(verbs, vec!["create", "list", "notify"]);
    }

    #[test]
    fn note_kinds_are_deduplicated() {
        let mut reg = VerbRegistry::new();
        reg.register(AlphaPack);
        reg.register(BetaPack);

        let kinds = reg.all_note_kinds();
        assert_eq!(kinds, vec!["memo", "log", "alert"]);
    }

    #[test]
    fn entity_kinds_are_deduplicated() {
        let mut reg = VerbRegistry::new();
        reg.register(AlphaPack);
        reg.register(BetaPack);

        let kinds = reg.all_entity_kinds();
        assert_eq!(kinds, vec!["widget", "gadget"]);
    }
}
