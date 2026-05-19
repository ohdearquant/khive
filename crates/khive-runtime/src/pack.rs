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

use std::sync::Arc;

use async_trait::async_trait;
use khive_gate::{ActorRef, AllowAllGate, AuditEvent, GateDecision, GateRef, GateRequest};
use khive_types::Namespace;
use serde_json::Value;

pub use khive_types::{EdgeEndpointRule, EndpointKind, VerbDef};

use crate::error::RuntimeError;
use crate::KhiveRuntime;

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

    /// Pack-extensible edge endpoint rules — must equal `<Self as Pack>::EDGE_RULES`.
    /// Defaults to empty so existing packs that don't extend the edge contract
    /// can ignore it (ADR-031).
    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        &[]
    }

    /// Optional per-kind hook for shared CRUD specialization (ADR-030).
    ///
    /// When a kind is owned by this pack (declared in `note_kinds()` or
    /// `entity_kinds()`), returning `Some(hook)` opts that kind into
    /// pack-specific behavior — defaults, derived properties, side-effect
    /// edges — through the shared `create` path. Returning `None` keeps
    /// the kind as plain storage with no specialization.
    fn kind_hook(&self, _kind: &str) -> Option<Arc<dyn KindHook>> {
        None
    }

    /// Dispatch a verb call. Returns serialized JSON response.
    ///
    /// The `registry` parameter gives the handler access to the merged
    /// vocabulary and kind hooks across all loaded packs (ADR-030).
    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError>;
}

/// Per-kind specialization for shared CRUD (ADR-030).
///
/// Packs implement `KindHook` for kinds they own that need:
/// - **Defaults** filled into create args (e.g. `status="inbox"` for tasks)
/// - **Derived properties** computed from args (e.g. salience from priority)
/// - **Side-effect writes** after the storage commit (e.g. `depends_on` edges)
///
/// Hooks are stateless from the framework's perspective — they receive the
/// runtime as a method parameter and operate on the args `Value` directly.
/// The pack registers them via [`PackRuntime::kind_hook`].
///
/// Lifecycle verbs (e.g. gtd's `complete`, `transition`) remain pack-owned
/// verbs and do not flow through this trait — only the create path does.
#[async_trait]
pub trait KindHook: Send + Sync + std::fmt::Debug {
    /// Mutate args before the storage write. Fill defaults, normalize values,
    /// rearrange user-facing fields into the storage shape expected by the
    /// shared CRUD handler.
    ///
    /// Returning an error aborts the create call (no storage write happens).
    async fn prepare_create(
        &self,
        runtime: &KhiveRuntime,
        args: &mut Value,
    ) -> Result<(), RuntimeError>;

    /// Fire side effects after a successful storage write — graph edges,
    /// derived observations, etc. The newly created record's UUID is passed
    /// so the hook can attach metadata referencing it.
    ///
    /// Errors here are **logged but not propagated** — the storage write has
    /// already succeeded; failing the call would mislead the caller.
    /// Implementations should `tracing::warn!` and return `Ok(())` for
    /// best-effort side effects.
    async fn after_create(
        &self,
        runtime: &KhiveRuntime,
        id: uuid::Uuid,
        args: &Value,
    ) -> Result<(), RuntimeError>;
}

/// Builder for constructing a `VerbRegistry`.
///
/// Packs are registered here; once `.build()` is called the registry is
/// immutable and cheaply cloneable.
pub struct VerbRegistryBuilder {
    packs: Vec<Box<dyn PackRuntime>>,
    gate: GateRef,
    default_namespace: String,
}

impl VerbRegistryBuilder {
    pub fn new() -> Self {
        Self {
            packs: Vec::new(),
            gate: std::sync::Arc::new(AllowAllGate),
            default_namespace: Namespace::default_ns().as_str().to_string(),
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

    /// Set the namespace surfaced to the gate when a verb does not carry an
    /// explicit `namespace` argument. Transports should plumb the runtime's
    /// `default_namespace` so the gate's `input.namespace` always reflects
    /// the operation's true tenant (ADR-029 + ADR-007).
    pub fn with_default_namespace(&mut self, ns: impl Into<String>) -> &mut Self {
        self.default_namespace = ns.into();
        self
    }

    /// Consume the builder and produce an immutable, cloneable registry.
    pub fn build(self) -> VerbRegistry {
        VerbRegistry {
            packs: std::sync::Arc::new(self.packs),
            gate: self.gate,
            default_namespace: self.default_namespace,
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
    default_namespace: String,
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
    /// operation's namespace — pulled from `params["namespace"]` when present
    /// (including an explicit empty string, which `KhiveRuntime::ns` also
    /// preserves), otherwise the registry's default namespace (configured via
    /// [`VerbRegistryBuilder::with_default_namespace`]). Gate-visible
    /// namespace and runtime-visible namespace MUST stay aligned; coercing an
    /// empty string here while the runtime keeps `""` would create an
    /// authorization/audit blind spot on the field ADR-029 declares public.
    /// Transports that have richer caller context (auth headers, session
    /// info) will gain a sibling dispatch path in a follow-up.
    pub async fn dispatch(&self, verb: &str, params: Value) -> Result<Value, RuntimeError> {
        let ns_str = params
            .get("namespace")
            .and_then(Value::as_str)
            .unwrap_or(&self.default_namespace);
        let gate_req = GateRequest::new(
            ActorRef::anonymous(),
            Namespace::new(ns_str),
            verb,
            params.clone(),
        );
        // Consult the gate and emit a structured audit event (ADR-033).
        // In v0.2 the gate is advisory — Deny is logged but does not block.
        // The audit event is emitted via tracing::info! as structured JSON.
        // Storage-backed emission (EventStore::append_event) is deferred to v0.3
        // when VerbRegistry gains a runtime handle; see ADR-033 §"Implementation Status".
        match self.gate.check(&gate_req) {
            Ok(decision) => {
                if matches!(decision, GateDecision::Deny { .. }) {
                    tracing::warn!(
                        verb,
                        reason = %match &decision { GateDecision::Deny { reason } => reason.as_str(), _ => "" },
                        "gate deny (advisory in v0.2; not enforced)"
                    );
                }
                let audit = AuditEvent::from_check(&gate_req, &decision, self.gate.impl_name());
                tracing::info!(
                    audit_event = %serde_json::to_string(&audit)
                        .unwrap_or_else(|_| "{\"error\":\"serialize\"}".into()),
                    "gate.check"
                );
            }
            Err(err) => {
                tracing::warn!(verb, error = %err, "gate check failed (advisory)");
            }
        }

        for pack in self.packs.iter() {
            if pack.verbs().iter().any(|v| v.name == verb) {
                return pack.dispatch(verb, params, self).await;
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

    /// Find a kind hook (ADR-030) among the registered packs.
    ///
    /// Walks packs in registration order; the first pack that both owns the
    /// kind (declares it in `note_kinds()` or `entity_kinds()`) and returns
    /// a hook from `kind_hook(kind)` wins. Returns `None` if the kind is
    /// unknown to all packs or no owning pack registered a hook.
    pub fn find_kind_hook(&self, kind: &str) -> Option<Arc<dyn KindHook>> {
        for pack in self.packs.iter() {
            let owns = pack.note_kinds().contains(&kind) || pack.entity_kinds().contains(&kind);
            if owns {
                if let Some(hook) = pack.kind_hook(kind) {
                    return Some(hook);
                }
            }
        }
        None
    }

    /// All verb definitions across all registered packs.
    ///
    /// Returned with `'static` lifetime since pack verbs are `&'static [VerbDef]`
    /// constants — callers can keep the slice references beyond the registry's
    /// borrow.
    pub fn all_verbs(&self) -> Vec<&'static VerbDef> {
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

    /// All pack-declared edge endpoint rules across registered packs (ADR-031).
    ///
    /// Order follows pack registration; duplicates are *not* deduplicated —
    /// validation only checks membership, and an exact-duplicate rule is a
    /// harmless restatement.
    pub fn all_edge_rules(&self) -> Vec<EdgeEndpointRule> {
        self.packs
            .iter()
            .flat_map(|p| p.edge_rules().iter().copied())
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
        async fn dispatch(
            &self,
            verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
        ) -> Result<Value, RuntimeError> {
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
        async fn dispatch(
            &self,
            verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
        ) -> Result<Value, RuntimeError> {
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

    // Captures the namespace each call sees so we can assert what the gate
    // actually receives — codex round-1 caught us hard-wiring `default_ns()`.
    #[derive(Default, Debug)]
    struct NamespaceCapturingGate {
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl Gate for NamespaceCapturingGate {
        fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
            self.seen
                .lock()
                .unwrap()
                .push(req.namespace.as_str().to_string());
            Ok(GateDecision::allow())
        }
    }

    #[tokio::test]
    async fn dispatch_propagates_params_namespace_to_gate() {
        let gate = Arc::new(NamespaceCapturingGate::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        builder.with_default_namespace("tenant-x");
        let reg = builder.build();

        // Explicit namespace in params wins.
        reg.dispatch("list", serde_json::json!({"namespace": "tenant-y"}))
            .await
            .unwrap();
        // Missing namespace → registry default.
        reg.dispatch("list", Value::Null).await.unwrap();
        // Explicit empty namespace string is preserved (it is what
        // `KhiveRuntime::ns` would also see). Gate and runtime MUST agree on
        // the namespace they observe; coercing here while the runtime
        // continues to honor `""` would create an audit blind spot.
        reg.dispatch("list", serde_json::json!({"namespace": ""}))
            .await
            .unwrap();

        let seen = gate.seen.lock().unwrap().clone();
        assert_eq!(seen, vec!["tenant-y", "tenant-x", ""]);
    }

    #[tokio::test]
    async fn dispatch_falls_back_to_local_when_no_default_set() {
        // Builder default mirrors `Namespace::default_ns()`.
        let gate = Arc::new(NamespaceCapturingGate::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        let reg = builder.build();

        reg.dispatch("list", Value::Null).await.unwrap();
        let seen = gate.seen.lock().unwrap().clone();
        assert_eq!(seen, vec!["local"]);
    }

    // ---- Audit event emission (ADR-033) ----

    use khive_gate::{AuditDecision, AuditEvent, Obligation};

    /// A gate that records every audit event emitted via from_check.
    #[derive(Default, Debug)]
    struct AuditCapturingGate {
        events: std::sync::Mutex<Vec<AuditEvent>>,
        deny_verb: Option<&'static str>,
    }

    impl Gate for AuditCapturingGate {
        fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
            let decision = if Some(req.verb.as_str()) == self.deny_verb {
                GateDecision::deny("test deny")
            } else {
                GateDecision::allow_with(vec![Obligation::Audit {
                    tag: format!("{}.check", req.verb),
                }])
            };
            // Capture what dispatch will also emit.
            let ev = AuditEvent::from_check(req, &decision, self.impl_name());
            self.events.lock().unwrap().push(ev);
            Ok(decision)
        }

        fn impl_name(&self) -> &'static str {
            "AuditCapturingGate"
        }
    }

    #[tokio::test]
    async fn dispatch_emits_one_audit_event_per_call() {
        let gate = Arc::new(AuditCapturingGate::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        let reg = builder.build();

        reg.dispatch("list", Value::Null).await.unwrap();
        reg.dispatch("create", Value::Null).await.unwrap();

        let evs = gate.events.lock().unwrap();
        assert_eq!(evs.len(), 2, "exactly one audit event per dispatch call");
    }

    #[tokio::test]
    async fn dispatch_audit_event_allow_carries_obligations() {
        let gate = Arc::new(AuditCapturingGate::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        let reg = builder.build();

        reg.dispatch("list", Value::Null).await.unwrap();

        let evs = gate.events.lock().unwrap();
        let ev = &evs[0];
        assert_eq!(ev.verb, "list");
        assert_eq!(ev.decision, AuditDecision::Allow);
        assert!(ev.deny_reason.is_none());
        assert_eq!(ev.obligations.len(), 1);
        assert_eq!(ev.gate_impl, "AuditCapturingGate");
    }

    #[tokio::test]
    async fn dispatch_audit_event_deny_carries_reason() {
        let gate = Arc::new(AuditCapturingGate {
            events: Default::default(),
            deny_verb: Some("create"),
        });
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        let reg = builder.build();

        // Gate denies but dispatch still succeeds (advisory v0.2).
        let res = reg.dispatch("create", Value::Null).await.unwrap();
        assert_eq!(res["pack"], "alpha");

        let evs = gate.events.lock().unwrap();
        let ev = &evs[0];
        assert_eq!(ev.verb, "create");
        assert_eq!(ev.decision, AuditDecision::Deny);
        assert_eq!(ev.deny_reason.as_deref(), Some("test deny"));
        assert!(ev.obligations.is_empty());
    }

    #[tokio::test]
    async fn dispatch_audit_event_fields_match_gate_request() {
        let gate = Arc::new(AuditCapturingGate::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        builder.with_default_namespace("tenant-z");
        let reg = builder.build();

        reg.dispatch("list", serde_json::json!({"namespace": "tenant-q"}))
            .await
            .unwrap();

        let evs = gate.events.lock().unwrap();
        let ev = &evs[0];
        // Namespace from params wins (ADR-029 alignment rule).
        assert_eq!(ev.namespace, "tenant-q");
        assert_eq!(ev.verb, "list");
        assert_eq!(ev.actor.kind, "anonymous");
    }

    // ---- Audit tracing emission (ADR-033 §"Emission site") ----
    //
    // The AuditCapturingGate tests above prove that AuditEvent::from_check is
    // called with the right inputs, but they observe the event *inside* the
    // gate impl — they would still pass if dispatch's
    // `tracing::info!(audit_event = ..., "gate.check")` were deleted or
    // renamed. The tests below install a capture Layer and assert on the
    // actual tracing event surfaced from dispatch. This locks the public
    // observability contract from ADR-033: one `gate.check` info event per
    // dispatch, carrying an `audit_event` field that round-trips back to an
    // `AuditEvent`.

    use std::sync::Mutex as StdMutex;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Layer;

    #[derive(Clone, Debug, Default)]
    struct CapturedEvent {
        message: Option<String>,
        audit_event: Option<String>,
    }

    #[derive(Default)]
    struct CapturedEventVisitor(CapturedEvent);

    impl Visit for CapturedEventVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            match field.name() {
                "message" => self.0.message = Some(value.to_string()),
                "audit_event" => self.0.audit_event = Some(value.to_string()),
                _ => {}
            }
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            // `tracing::info!(audit_event = %expr, "msg")` records via the
            // Display-wrapped Debug path, so we receive the JSON string here.
            // `"msg"` literal records as a `message` field via `record_debug`
            // with a quoted Debug representation; strip the surrounding quotes
            // so the captured message matches the source.
            let formatted = format!("{value:?}");
            let cleaned = formatted
                .trim_start_matches('"')
                .trim_end_matches('"')
                .to_string();
            match field.name() {
                "message" => self.0.message = Some(cleaned),
                "audit_event" => self.0.audit_event = Some(cleaned),
                _ => {}
            }
        }
    }

    struct CaptureLayer(Arc<StdMutex<Vec<CapturedEvent>>>);

    impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut visitor = CapturedEventVisitor::default();
            event.record(&mut visitor);
            self.0.lock().unwrap().push(visitor.0);
        }
    }

    /// Run an async block under a scoped tracing subscriber and return the
    /// events captured during the run. Uses a current-thread tokio runtime so
    /// the thread-local subscriber set by `with_default` covers every task
    /// spawned in the body.
    fn capture_dispatch_events<Fut>(future: Fut) -> Vec<CapturedEvent>
    where
        Fut: std::future::Future<Output = ()>,
    {
        let captured: Arc<StdMutex<Vec<CapturedEvent>>> = Arc::new(StdMutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(CaptureLayer(Arc::clone(&captured)));

        tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build current-thread tokio runtime");
            rt.block_on(future);
        });

        let guard = captured.lock().unwrap();
        guard.clone()
    }

    /// Pull every captured event whose `message` matches `"gate.check"`.
    fn gate_check_events(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
        events
            .iter()
            .filter(|e| e.message.as_deref() == Some("gate.check"))
            .collect()
    }

    #[test]
    fn dispatch_tracing_emits_one_gate_check_event_on_allow() {
        let events = capture_dispatch_events(async {
            let mut builder = VerbRegistryBuilder::new();
            builder.register(AlphaPack);
            builder.with_gate(Arc::new(AllowAllGate));
            builder.with_default_namespace("tenant-default");
            let reg = builder.build();
            reg.dispatch("list", serde_json::json!({"namespace": "tenant-q"}))
                .await
                .unwrap();
        });

        let gate_events = gate_check_events(&events);
        assert_eq!(
            gate_events.len(),
            1,
            "exactly one gate.check tracing event per dispatch (allow); got {gate_events:?}"
        );
        let payload = gate_events[0]
            .audit_event
            .as_ref()
            .expect("gate.check event must carry an audit_event field");
        let audit: khive_gate::AuditEvent =
            serde_json::from_str(payload).expect("audit_event payload must decode to AuditEvent");
        assert_eq!(audit.decision, AuditDecision::Allow);
        assert_eq!(audit.verb, "list");
        assert_eq!(audit.namespace, "tenant-q");
        assert_eq!(audit.gate_impl, "AllowAllGate");
        assert!(
            audit.deny_reason.is_none(),
            "deny_reason must be None on Allow"
        );
    }

    #[test]
    fn dispatch_tracing_emits_gate_check_event_with_deny_payload() {
        #[derive(Debug)]
        struct AlwaysDenyGate;
        impl Gate for AlwaysDenyGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                Ok(GateDecision::deny("denied by test gate"))
            }
            fn impl_name(&self) -> &'static str {
                "AlwaysDenyGate"
            }
        }

        let events = capture_dispatch_events(async {
            let mut builder = VerbRegistryBuilder::new();
            builder.register(AlphaPack);
            builder.with_gate(Arc::new(AlwaysDenyGate));
            let reg = builder.build();
            // Advisory in v0.2 — dispatch succeeds even when the gate denies.
            reg.dispatch("create", serde_json::Value::Null)
                .await
                .unwrap();
        });

        let gate_events = gate_check_events(&events);
        assert_eq!(
            gate_events.len(),
            1,
            "exactly one gate.check tracing event per dispatch (deny); got {gate_events:?}"
        );
        let payload = gate_events[0]
            .audit_event
            .as_ref()
            .expect("gate.check event must carry an audit_event field on Deny");
        let audit: khive_gate::AuditEvent =
            serde_json::from_str(payload).expect("audit_event payload must decode to AuditEvent");
        assert_eq!(audit.decision, AuditDecision::Deny);
        assert_eq!(audit.deny_reason.as_deref(), Some("denied by test gate"));
        assert_eq!(audit.gate_impl, "AlwaysDenyGate");
        // Wire-shape rule from ADR-033: obligations is always serialized as an
        // array, empty on Deny. Round-trip back through serde_json::Value to
        // confirm the field exists on the wire and is `[]`, not missing.
        let payload_json: serde_json::Value =
            serde_json::from_str(payload).expect("payload must be valid JSON");
        assert_eq!(
            payload_json["obligations"],
            serde_json::Value::Array(Vec::new()),
            "obligations must be `[]` on Deny on the tracing payload, not omitted"
        );
    }
}
