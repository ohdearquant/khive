use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use khive_runtime::{
    ContextContributor, ContextRequest, ContextSlice, NamespaceToken, PackRuntime, RuntimeError,
    ScoreSemantics, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::Direction;
use khive_types::{HandlerDef, Pack, VerbCategory, Visibility};
use serde_json::{json, Value};

// This pack deliberately implements only the previously required methods.
struct PlainPack;

impl Pack for PlainPack {
    const NAME: &'static str = "plain_context_fixture";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
}

#[async_trait]
impl PackRuntime for PlainPack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        panic!("the default-only pack must never be dispatched")
    }
}

struct RecordingContributor {
    source: &'static str,
    calls: AtomicUsize,
    seen: Mutex<Option<(ContextRequest, String)>>,
    fail: bool,
}

impl RecordingContributor {
    fn new(source: &'static str, fail: bool) -> Arc<Self> {
        Arc::new(Self {
            source,
            calls: AtomicUsize::new(0),
            seen: Mutex::new(None),
            fail,
        })
    }
}

#[async_trait]
impl ContextContributor for RecordingContributor {
    fn source_pack(&self) -> &'static str {
        self.source
    }

    async fn contribute(
        &self,
        req: &ContextRequest,
        token: &NamespaceToken,
    ) -> Result<Vec<ContextSlice>, RuntimeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.seen.lock().unwrap() = Some((req.clone(), token.namespace().as_str().to_owned()));
        tokio::task::yield_now().await;
        if self.fail {
            return Err(RuntimeError::InvalidInput(
                "contributor fixture refusal".into(),
            ));
        }
        Ok(vec![ContextSlice {
            source_pack: self.source_pack(),
            kind: "fixture-record".into(),
            id: req.entity_ids[0].clone(),
            content: json!({ "query": req.query, "nested": [1, { "owned": true }] }),
            score: Some(1.75),
            score_semantics: ScoreSemantics::DecayWeighted,
        }])
    }
}

struct ProviderPack<const DEPENDENT: bool> {
    contributor: Arc<dyn ContextContributor>,
    dispatches: Arc<AtomicUsize>,
    request: ContextRequest,
}

impl<const DEPENDENT: bool> Pack for ProviderPack<DEPENDENT> {
    const NAME: &'static str = if DEPENDENT { "acontext" } else { "zcontext" };
    const REQUIRES: &'static [&'static str] = if DEPENDENT { &["zcontext"] } else { &[] };
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: if DEPENDENT {
            "acontext.run"
        } else {
            "zcontext.run"
        },
        description: "Test the optional context capability with a dispatch-minted token",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }];
}

#[async_trait]
impl<const DEPENDENT: bool> PackRuntime for ProviderPack<DEPENDENT> {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    fn requires(&self) -> &'static [&'static str] {
        Self::REQUIRES
    }
    fn context_contributor(&self) -> Option<Arc<dyn ContextContributor>> {
        Some(self.contributor.clone())
    }
    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        let contributor: Arc<dyn ContextContributor> = self.context_contributor().unwrap();
        let slices = contributor.contribute(&self.request, token).await?;
        Ok(json!(slices))
    }
}

fn request() -> ContextRequest {
    ContextRequest {
        query: Some("unaltered query".into()),
        entity_ids: vec!["opaque-first-id".into(), "second-id".into()],
        consumer_kind: "caller-defined-consumer".into(),
        budget_hint: 1234,
        hops: 4,
        fanout: 7,
        direction: Direction::Both,
        relations: vec!["depends_on".into(), "relates_to".into()],
    }
}

fn provider<const DEPENDENT: bool>(
    contributor: Arc<dyn ContextContributor>,
    dispatches: Arc<AtomicUsize>,
) -> ProviderPack<DEPENDENT> {
    ProviderPack {
        contributor,
        dispatches,
        request: request(),
    }
}

#[test]
fn default_capability_and_empty_registry_have_no_contributors() {
    assert!(PlainPack.context_contributor().is_none());
    assert!(VerbRegistryBuilder::new()
        .build()
        .unwrap()
        .context_contributors()
        .is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(PlainPack);
    assert!(builder.build().unwrap().context_contributors().is_empty());
}

#[test]
fn discovery_preserves_arc_identity_and_does_not_execute_work() {
    let source = RecordingContributor::new("zcontext", false);
    let capability: Arc<dyn ContextContributor> = source.clone();
    let dispatches = Arc::new(AtomicUsize::new(0));
    let mut builder = VerbRegistryBuilder::new();
    builder
        .register(PlainPack)
        .register(provider::<false>(capability.clone(), dispatches.clone()));
    let registry = builder.build().unwrap();
    let cloned = registry.clone();
    for view in [&registry, &cloned] {
        let found = view.context_contributors();
        assert_eq!(found.len(), 1);
        assert!(Arc::ptr_eq(&found[0], &capability));
        assert_eq!(found[0].source_pack(), "zcontext");
    }
    assert_eq!(source.calls.load(Ordering::SeqCst), 0);
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
    assert!(source.seen.lock().unwrap().is_none());
}

#[test]
fn discovery_uses_topological_order_not_insertion_or_lexical_order() {
    let base: Arc<dyn ContextContributor> = RecordingContributor::new("zcontext", false);
    let dependent: Arc<dyn ContextContributor> = RecordingContributor::new("acontext", false);
    let dispatches = Arc::new(AtomicUsize::new(0));
    let mut builder = VerbRegistryBuilder::new();
    builder.register(provider::<true>(dependent.clone(), dispatches.clone()));
    builder.register(PlainPack);
    builder.register(provider::<false>(base.clone(), dispatches.clone()));
    let registry = builder.build().unwrap();
    for found in [
        registry.context_contributors(),
        registry.clone().context_contributors(),
    ] {
        assert_eq!(found.len(), 2);
        assert!(Arc::ptr_eq(&found[0], &base));
        assert!(Arc::ptr_eq(&found[1], &dependent));
        assert_eq!(
            found.iter().map(|c| c.source_pack()).collect::<Vec<_>>(),
            ["zcontext", "acontext"]
        );
    }
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dyn_contributor_receives_owned_request_and_real_dispatch_namespace() {
    let source = RecordingContributor::new("zcontext", false);
    let dispatches = Arc::new(AtomicUsize::new(0));
    let mut builder = VerbRegistryBuilder::new();
    builder.with_default_namespace("gate-default");
    builder.register(provider::<false>(source.clone(), dispatches.clone()));
    let registry = builder.build().unwrap();
    let result = registry
        .dispatch("zcontext.run", json!({ "namespace": "context-fixture" }))
        .await
        .unwrap();
    drop(registry);
    assert_eq!(
        result,
        json!([{
            "source_pack": "zcontext", "kind": "fixture-record", "id": "opaque-first-id",
            "content": { "query": "unaltered query", "nested": [1, { "owned": true }] },
            "score": 1.75, "score_semantics": "decay_weighted"
        }])
    );
    let (seen, namespace) = source.seen.lock().unwrap().clone().unwrap();
    assert_eq!(namespace, "context-fixture");
    assert_eq!(seen.query, Some("unaltered query".into()));
    assert_eq!(seen.entity_ids, ["opaque-first-id", "second-id"]);
    assert_eq!(seen.consumer_kind, "caller-defined-consumer");
    assert_eq!(seen.budget_hint, 1234);
    assert_eq!((seen.hops, seen.fanout), (4, 7));
    assert_eq!(seen.direction, Direction::Both);
    assert_eq!(seen.relations, ["depends_on", "relates_to"]);
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn contributor_runtime_error_propagates_through_actual_dispatch() {
    let source = RecordingContributor::new("zcontext", true);
    let dispatches = Arc::new(AtomicUsize::new(0));
    let mut builder = VerbRegistryBuilder::new();
    builder.with_default_namespace("gate-default");
    builder.register(provider::<false>(source.clone(), dispatches.clone()));
    let error = builder
        .build()
        .unwrap()
        .dispatch("zcontext.run", json!({ "namespace": "context-fixture" }))
        .await
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::InvalidInput(message) if message == "contributor fixture refusal")
    );
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(
        source.seen.lock().unwrap().as_ref().unwrap().1,
        "context-fixture"
    );
}

#[test]
fn score_semantics_have_exact_closed_wire_tokens() {
    for (semantics, expected) in [
        (ScoreSemantics::DecayWeighted, "decay_weighted"),
        (ScoreSemantics::Rerank, "rerank"),
        (ScoreSemantics::GraphProximity, "graph_proximity"),
        (ScoreSemantics::Relevance, "relevance"),
        (ScoreSemantics::None, "none"),
    ] {
        assert_eq!(serde_json::to_value(semantics).unwrap(), json!(expected));
    }
}

#[test]
fn owned_slice_serialization_preserves_optional_and_partition_local_scores() {
    let mut slice = khive_runtime::context::ContextSlice {
        source_pack: "fixture",
        kind: "kind".into(),
        id: "id".into(),
        content: json!({ "opaque": [null, "body"] }),
        score: None,
        score_semantics: ScoreSemantics::None,
    };
    assert_eq!(
        serde_json::to_value(&slice).unwrap(),
        json!({
            "source_pack": "fixture", "kind": "kind", "id": "id",
            "content": { "opaque": [null, "body"] }, "score": null, "score_semantics": "none"
        })
    );
    for score in [-2.5, 1.75] {
        slice.score = Some(score);
        slice.score_semantics = ScoreSemantics::Rerank;
        assert_eq!(
            serde_json::to_value(slice.clone()).unwrap()["score"],
            json!(score)
        );
    }
}
