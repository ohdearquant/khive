//! Pack runtime trait and verb registry (ADR-025 step 2).
//!
//! Packs register verbs into the runtime. The registry routes verb calls
//! to the pack that declares them.
//!
//! `Pack` (in khive-types) uses const associated items which are not
//! object-safe. `PackRuntime` mirrors that metadata as methods so the
//! registry can store packs as trait objects. See ADR-025 §PackRuntime.
//!
//! Lifecycle: build with `VerbRegistryBuilder`, then call `.build()` to
//! get a cheaply-cloneable `VerbRegistry`. Registration is only possible
//! through the builder.

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
/// Registration requires `P: Pack + PackRuntime` — the compiler enforces
/// that every runtime pack also declares its vocabulary via `Pack`.
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

/// Builder for constructing a `VerbRegistry`.
///
/// Packs are registered here; once `.build()` is called the registry is
/// immutable and cheaply cloneable.
pub struct VerbRegistryBuilder {
    packs: Vec<Box<dyn PackRuntime>>,
}

impl VerbRegistryBuilder {
    pub fn new() -> Self {
        Self { packs: Vec::new() }
    }

    /// Register a pack. The bound `P: Pack + PackRuntime` ensures the pack
    /// declares vocabulary via `Pack` consts alongside runtime dispatch.
    pub fn register<P: khive_types::Pack + PackRuntime + 'static>(&mut self, pack: P) -> &mut Self {
        self.packs.push(Box::new(pack));
        self
    }

    /// Consume the builder and produce an immutable, cloneable registry.
    pub fn build(self) -> VerbRegistry {
        VerbRegistry {
            packs: std::sync::Arc::new(self.packs),
        }
    }
}

impl Default for VerbRegistryBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Immutable registry that dispatches verb calls to registered packs.
///
/// Clone is cheap (Arc-wrapped). Constructed via `VerbRegistryBuilder`.
#[derive(Clone)]
pub struct VerbRegistry {
    packs: std::sync::Arc<Vec<Box<dyn PackRuntime>>>,
}

impl VerbRegistry {
    /// Dispatch a verb to the first pack that handles it.
    ///
    /// When multiple packs declare the same verb, the first registered pack wins.
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

#[cfg(test)]
mod tests {
    use super::*;
    use khive_types::Pack;

    struct AlphaPack;

    impl Pack for AlphaPack {
        const NAME: &'static str = "alpha";
        const NOTE_KINDS: &'static [&'static str] = &["memo", "log"];
        const ENTITY_KINDS: &'static [&'static str] = &["widget"];
        const VERBS: &'static [VerbDef] = &[
            VerbDef {
                name: "create",
                description: "create a widget",
            },
            VerbDef {
                name: "list",
                description: "list widgets",
            },
        ];
    }

    #[async_trait]
    impl PackRuntime for AlphaPack {
        fn name(&self) -> &str {
            AlphaPack::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            AlphaPack::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            AlphaPack::ENTITY_KINDS
        }
        fn verbs(&self) -> &'static [VerbDef] {
            AlphaPack::VERBS
        }
        async fn dispatch(&self, verb: &str, _params: Value) -> Result<Value, RuntimeError> {
            Ok(serde_json::json!({ "pack": "alpha", "verb": verb }))
        }
    }

    struct BetaPack;

    impl Pack for BetaPack {
        const NAME: &'static str = "beta";
        const NOTE_KINDS: &'static [&'static str] = &["log", "alert"];
        const ENTITY_KINDS: &'static [&'static str] = &["widget", "gadget"];
        const VERBS: &'static [VerbDef] = &[
            VerbDef {
                name: "notify",
                description: "send alert",
            },
            VerbDef {
                name: "create",
                description: "create a gadget",
            },
        ];
    }

    #[async_trait]
    impl PackRuntime for BetaPack {
        fn name(&self) -> &str {
            BetaPack::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            BetaPack::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            BetaPack::ENTITY_KINDS
        }
        fn verbs(&self) -> &'static [VerbDef] {
            BetaPack::VERBS
        }
        async fn dispatch(&self, verb: &str, _params: Value) -> Result<Value, RuntimeError> {
            Ok(serde_json::json!({ "pack": "beta", "verb": verb }))
        }
    }

    fn build_registry() -> VerbRegistry {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.register(BetaPack);
        builder.build()
    }

    #[tokio::test]
    async fn dispatch_routes_to_correct_pack() {
        let reg = build_registry();

        let res = reg.dispatch("list", Value::Null).await.unwrap();
        assert_eq!(res["pack"], "alpha");

        let res = reg.dispatch("notify", Value::Null).await.unwrap();
        assert_eq!(res["pack"], "beta");
    }

    #[tokio::test]
    async fn dispatch_first_registered_wins_on_collision() {
        let reg = build_registry();

        let res = reg.dispatch("create", Value::Null).await.unwrap();
        assert_eq!(res["pack"], "alpha", "first registered pack wins");
    }

    #[tokio::test]
    async fn dispatch_unknown_verb_returns_error() {
        let reg = build_registry();

        let err = reg.dispatch("explode", Value::Null).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("explode"));
        assert!(msg.contains("create"));
    }

    #[test]
    fn all_verbs_aggregates_across_packs() {
        let reg = build_registry();
        let verbs: Vec<&str> = reg.all_verbs().iter().map(|v| v.name).collect();
        assert_eq!(verbs, vec!["create", "list", "notify", "create"]);
    }

    #[test]
    fn note_kinds_are_deduplicated() {
        let reg = build_registry();
        let kinds = reg.all_note_kinds();
        assert_eq!(kinds, vec!["memo", "log", "alert"]);
    }

    #[test]
    fn entity_kinds_are_deduplicated() {
        let reg = build_registry();
        let kinds = reg.all_entity_kinds();
        assert_eq!(kinds, vec!["widget", "gadget"]);
    }
}
