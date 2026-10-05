use super::*;
use async_trait::async_trait;
use khive_types::Pack;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;

struct SimplePack;

impl Pack for SimplePack {
    const NAME: &'static str = "simple";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "ping",
        description: "ping",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for SimplePack {
    fn name(&self) -> &str {
        SimplePack::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        SimplePack::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        SimplePack::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        SimplePack::HANDLERS
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Ok(serde_json::json!({ "verb": verb }))
    }
}

struct RecallPack;

impl Pack for RecallPack {
    const NAME: &'static str = "memory";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "memory.recall",
        description: "test recall",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for RecallPack {
    fn name(&self) -> &str {
        RecallPack::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        RecallPack::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        RecallPack::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        RecallPack::HANDLERS
    }
    async fn dispatch(
        &self,
        _verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        let hit = serde_json::json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "served_by_profile_id": "custom-recall-v1",
            "serve_attribution": "profile",
        });
        if params.get("verbose").and_then(Value::as_bool) == Some(true) {
            Ok(serde_json::json!({"results": [hit]}))
        } else {
            Ok(serde_json::json!([hit]))
        }
    }
}

#[derive(Default)]
struct EventCapturingHook {
    event: StdMutex<Option<Event>>,
}

#[async_trait]
impl DispatchHook for EventCapturingHook {
    async fn on_dispatch(&self, view: &EventView) {
        *self.event.lock().unwrap() = Some(view.event.clone());
    }
}

/// Hook that counts calls and records the last verb seen.
#[derive(Default)]
struct CountingHook {
    calls: AtomicUsize,
    last_verb: StdMutex<String>,
}

#[async_trait]
impl DispatchHook for CountingHook {
    async fn on_dispatch(&self, view: &EventView) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_verb.lock().unwrap() = view.event.verb.clone();
    }
}

#[tokio::test]
async fn dispatch_hook_fires_on_successful_dispatch() {
    let hook = Arc::new(CountingHook::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(SimplePack);
    builder.with_dispatch_hook(hook.clone());
    let reg = builder.build().expect("registry builds");

    reg.dispatch("ping", Value::Null).await.unwrap();

    assert_eq!(
        hook.calls.load(Ordering::SeqCst),
        1,
        "hook must fire once per successful dispatch"
    );
    assert_eq!(
        hook.last_verb.lock().unwrap().as_str(),
        "ping",
        "hook event must carry the dispatched verb"
    );
}

#[tokio::test]
async fn dispatch_hook_fires_multiple_times() {
    let hook = Arc::new(CountingHook::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(SimplePack);
    builder.with_dispatch_hook(hook.clone());
    let reg = builder.build().expect("registry builds");

    reg.dispatch("ping", Value::Null).await.unwrap();
    reg.dispatch("ping", Value::Null).await.unwrap();
    reg.dispatch("ping", Value::Null).await.unwrap();

    assert_eq!(
        hook.calls.load(Ordering::SeqCst),
        3,
        "hook must fire once per successful dispatch"
    );
}

#[tokio::test]
async fn recall_hook_copies_serve_attribution_from_bare_and_verbose_results() {
    let hook = Arc::new(EventCapturingHook::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(RecallPack);
    builder.with_dispatch_hook(hook.clone());
    let reg = builder.build().expect("registry builds");

    for params in [serde_json::json!({}), serde_json::json!({"verbose": true})] {
        reg.dispatch("memory.recall", params)
            .await
            .expect("recall dispatch");
        let event = hook.event.lock().unwrap().clone().expect("hook event");
        assert!(
            event.target_id.is_some(),
            "first recall id must become target"
        );
        assert_eq!(
            event.payload["served_by_profile_id"],
            serde_json::json!("custom-recall-v1")
        );
        assert_eq!(
            event.payload["serve_attribution"],
            serde_json::json!("profile")
        );
    }
}

#[tokio::test]
async fn dispatch_hook_does_not_fire_on_unknown_verb() {
    let hook = Arc::new(CountingHook::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(SimplePack);
    builder.with_dispatch_hook(hook.clone());
    let reg = builder.build().expect("registry builds");

    let _ = reg.dispatch("nonexistent", Value::Null).await;

    assert_eq!(
        hook.calls.load(Ordering::SeqCst),
        0,
        "hook must NOT fire for unknown verb (dispatch returns error)"
    );
}

#[tokio::test]
async fn dispatch_hook_does_not_fire_on_gate_deny() {
    use khive_gate::{Gate, GateDecision, GateError};

    #[derive(Debug)]
    struct AlwaysDenyGate;
    impl Gate for AlwaysDenyGate {
        fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
            Ok(GateDecision::deny("test deny"))
        }
    }

    let hook = Arc::new(CountingHook::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(SimplePack);
    builder.with_gate(Arc::new(AlwaysDenyGate));
    builder.with_dispatch_hook(hook.clone());
    let reg = builder.build().expect("registry builds");

    let err = reg.dispatch("ping", Value::Null).await.unwrap_err();
    assert!(matches!(err, RuntimeError::PermissionDenied { .. }));

    assert_eq!(
        hook.calls.load(Ordering::SeqCst),
        0,
        "hook must NOT fire when gate denies dispatch"
    );
}

#[tokio::test]
async fn dispatch_hook_event_carries_namespace_from_params() {
    let hook = Arc::new(CountingHook::default());

    #[derive(Default)]
    struct NsCapturingHook {
        ns: StdMutex<String>,
    }

    #[async_trait]
    impl DispatchHook for NsCapturingHook {
        async fn on_dispatch(&self, view: &EventView) {
            *self.ns.lock().unwrap() = view.event.namespace.clone();
        }
    }

    let ns_hook = Arc::new(NsCapturingHook::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(SimplePack);
    builder.with_dispatch_hook(ns_hook.clone());
    let reg = builder.build().expect("registry builds");

    reg.dispatch("ping", serde_json::json!({"namespace": "tenant-abc"}))
        .await
        .unwrap();

    assert_eq!(
        ns_hook.ns.lock().unwrap().as_str(),
        "tenant-abc",
        "dispatch hook event must carry the resolved namespace"
    );

    // Suppress unused-variable warning from the outer hook.
    drop(hook);
}

#[tokio::test]
async fn no_dispatch_hook_configured_dispatch_succeeds() {
    // Regression: registries without a hook must still work.
    let mut builder = VerbRegistryBuilder::new();
    builder.register(SimplePack);
    // No with_dispatch_hook call.
    let reg = builder.build().expect("registry builds");

    let res = reg.dispatch("ping", Value::Null).await.unwrap();
    assert_eq!(res["verb"], "ping");
}
