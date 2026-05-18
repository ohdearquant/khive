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
use khive_gate::{ActorRef, AllowAllGate, GateDecision, GateRef, GateRequest};
use khive_types::Namespace;
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
    gate: GateRef,
}

impl VerbRegistryBuilder {
    pub fn new() -> Self {
        Self {
            packs: Vec::new(),
            gate: std::sync::Arc::new(AllowAllGate),
        }
    }

    /// Register a pack. The bound `P: Pack + PackRuntime` ensures the pack
    /// declares vocabulary via `Pack` consts alongside runtime dispatch.
    pub fn register<P: khive_types::Pack + PackRuntime + 'static>(&mut self, pack: P) -> &mut Self {
        self.packs.push(Box::new(pack));
        self
    }

    /// Set the authorization gate consulted on every dispatch (ADR-029).
    ///
    /// Defaults to `AllowAllGate` if not set. In v0.2 the gate is **advisory** —
    /// deny decisions are logged via `tracing::warn!` but do not block dispatch.
    pub fn with_gate(&mut self, gate: GateRef) -> &mut Self {
        self.gate = gate;
        self
    }

    /// Consume the builder and produce an immutable, cloneable registry.
    pub fn build(self) -> VerbRegistry {
        VerbRegistry {
            packs: std::sync::Arc::new(self.packs),
            gate: self.gate,
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
    gate: GateRef,
}

impl VerbRegistry {
    /// Dispatch a verb to the first pack that handles it.
    ///
    /// When multiple packs declare the same verb, the first registered pack wins.
    ///
    /// The configured [`Gate`](khive_gate::Gate) is consulted before dispatch
    /// (ADR-029). In v0.2 the check is **advisory** — `Deny` decisions are
    /// logged via `tracing::warn!` but do not abort the call. v0.3 will make
    /// deny authoritative.
    ///
    /// The synthesized `GateRequest` carries `ActorRef::anonymous()` and the
    /// default namespace. Transports that have richer caller context (auth
    /// headers, session info) will gain a sibling dispatch path in a follow-up.
    pub async fn dispatch(&self, verb: &str, params: Value) -> Result<Value, RuntimeError> {
        let gate_req = GateRequest::new(
            ActorRef::anonymous(),
            Namespace::default_ns(),
            verb,
            params.clone(),
        );
        match self.gate.check(&gate_req) {
            Ok(GateDecision::Allow { .. }) => {}
            Ok(GateDecision::Deny { reason }) => {
                tracing::warn!(
                    verb,
                    reason = %reason,
                    "gate deny (advisory in v0.2; not enforced)"
                );
            }
            Err(err) => {
                tracing::warn!(verb, error = %err, "gate check failed (advisory)");
            }
        }
        // TODO(ADR-032): emit `EventKind::GateCheck` event for deny / audit-obligation cases.

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

    // ---- Gate wiring (ADR-029) ----

    use khive_gate::{Gate, GateError};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Default, Debug)]
    struct CountingGate {
        calls: AtomicUsize,
        deny_verb: Option<&'static str>,
    }

    impl Gate for CountingGate {
        fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if Some(req.verb.as_str()) == self.deny_verb {
                Ok(GateDecision::deny(format!("test deny for {}", req.verb)))
            } else {
                Ok(GateDecision::allow())
            }
        }
    }

    #[tokio::test]
    async fn dispatch_consults_the_gate() {
        let gate = Arc::new(CountingGate::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        let reg = builder.build();

        reg.dispatch("list", Value::Null).await.unwrap();
        reg.dispatch("create", Value::Null).await.unwrap();
        assert_eq!(
            gate.calls.load(Ordering::SeqCst),
            2,
            "gate should be consulted once per dispatch"
        );
    }

    #[tokio::test]
    async fn dispatch_proceeds_on_deny_advisory_in_v02() {
        let gate = Arc::new(CountingGate {
            calls: AtomicUsize::new(0),
            deny_verb: Some("create"),
        });
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        let reg = builder.build();

        // Gate denies — but dispatch proceeds because the gate is advisory.
        let res = reg.dispatch("create", Value::Null).await.unwrap();
        assert_eq!(res["pack"], "alpha");
        assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dispatch_uses_allow_all_gate_by_default() {
        // No `with_gate` call — builder should use `AllowAllGate` so dispatch works.
        let reg = build_registry();
        let res = reg.dispatch("list", Value::Null).await.unwrap();
        assert_eq!(res["pack"], "alpha");
    }
}
