//! Search execution shape must survive empty results and result-source filtering.
use std::sync::Arc;

use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_runtime::{
    EmbedderProvider, InterceptedDispatchResult, KhiveRuntime, Namespace, RuntimeConfig,
    RuntimeResult, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{Entity, EventFilter, PageRequest};
use khive_types::{EventKind, EventOutcome};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};

struct Provider;
struct Embeddings;
#[async_trait]
impl EmbedderProvider for Provider {
    fn name(&self) -> &str {
        "all-minilm-l6-v2"
    }
    fn dimensions(&self) -> usize {
        EmbeddingModel::AllMiniLmL6V2.dimensions()
    }
    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(Embeddings))
    }
}
#[async_trait]
impl EmbeddingService for Embeddings {
    async fn embed(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut vector = vec![0.0; model.dimensions()];
        vector[0] = 1.0;
        Ok(texts.iter().map(|_| vector.clone()).collect())
    }
    fn supports_model(&self, model: EmbeddingModel) -> bool {
        model == EmbeddingModel::AllMiniLmL6V2
    }
    fn name(&self) -> &'static str {
        "search-audit-fixture"
    }
}

fn fixture(hybrid: bool) -> (KhiveRuntime, VerbRegistry) {
    let rt = KhiveRuntime::new_for_test(RuntimeConfig {
        db_path: None,
        embedding_model: hybrid.then_some(EmbeddingModel::AllMiniLmL6V2),
        packs: vec!["kg".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    if hybrid {
        rt.register_embedder(Provider);
    }
    let token = rt.authorize(Namespace::local()).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_event_store(rt.events(&token).unwrap());
    builder.register(KgPack::new(rt.clone()));
    (rt, builder.build().unwrap())
}

async fn audit(rt: &KhiveRuntime, verb: &str) -> khive_storage::Event {
    let token = rt.authorize(Namespace::local()).unwrap();
    let page = rt
        .events(&token)
        .unwrap()
        .query_events(
            EventFilter {
                kinds: vec![EventKind::Audit],
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap();
    let mut rows: Vec<_> = page.items.into_iter().filter(|e| e.verb == verb).collect();
    assert_eq!(
        rows.len(),
        1,
        "one dispatch audit, not search_executed telemetry"
    );
    rows.pop().unwrap()
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn empty_entity_and_note_searches_record_actual_arms_not_the_source_filter() {
    for hybrid in [false, true] {
        for kind in ["entity", "note"] {
            let (rt, registry) = fixture(hybrid);
            let query = "privateauditneedle missingallterms";
            let result = registry
                .dispatch(
                    "search",
                    json!({
                        "kind": kind, "query": query, "text_mode": "all_terms", "limit": 7,
                        "source": "vector"
                    }),
                )
                .await
                .unwrap();
            assert_eq!(result, json!([]));
            let event = audit(&rt, "search").await;
            assert_eq!(event.outcome, EventOutcome::Success);
            assert_eq!(event.payload["arms"], if hybrid { "both" } else { "text" });
            assert_eq!(event.payload["match_mode"], "all_terms");
            assert_eq!(event.payload["effective_limit"], 7);
            assert_eq!(event.payload["result_count"], 0);
            assert!(!event.payload.to_string().contains(query));
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn populated_any_term_search_counts_final_rows_and_caps_public_limit() {
    let (rt, registry) = fixture(false);
    let token = rt.authorize(Namespace::local()).unwrap();
    let entity = Entity::new("local", "concept", "auditfixture");
    rt.entities(&token)
        .unwrap()
        .upsert_entity(entity.clone())
        .await
        .unwrap();
    rt.text(&token)
        .unwrap()
        .upsert_document(khive_runtime::curation::entity_fts_document(&entity))
        .await
        .unwrap();
    let result = registry.dispatch("search", json!({
        "kind": "entity", "query": "auditfixture absentword", "text_mode": "any_term", "limit": 999
    })).await.unwrap();
    assert_eq!(result.as_array().unwrap().len(), 1);
    let event = audit(&rt, "search").await;
    assert_eq!(event.payload["arms"], "text");
    assert_eq!(event.payload["match_mode"], "any_term");
    assert_eq!(event.payload["effective_limit"], 100);
    assert_eq!(event.payload["result_count"], 1);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn defaults_and_other_verbs_have_distinct_audit_shapes() {
    let (rt, registry) = fixture(false);
    registry
        .dispatch("search", json!({"kind":"entity", "query":"emptyneedle"}))
        .await
        .unwrap();
    let event = audit(&rt, "search").await;
    assert_eq!(event.payload["effective_limit"], 10);
    assert_eq!(event.payload["match_mode"], "all_terms");
    registry
        .dispatch("list", json!({"kind":"entity"}))
        .await
        .unwrap();
    let event = audit(&rt, "list").await;
    for key in ["match_mode", "arms", "effective_limit", "result_count"] {
        assert!(event.payload.get(key).is_none(), "{key}");
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unsuccessful_search_has_no_success_shape() {
    let (rt, registry) = fixture(false);
    assert!(registry
        .dispatch(
            "search",
            json!({"kind":"entity", "query":"needle", "text_mode":"invalid"})
        )
        .await
        .is_err());
    let event = audit(&rt, "search").await;
    assert_eq!(event.outcome, EventOutcome::Error);
    for key in ["match_mode", "arms", "effective_limit", "result_count"] {
        assert!(event.payload.get(key).is_none(), "{key}");
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn intercepted_joined_empty_hybrid_records_execution_without_result_provenance() {
    let (rt, registry) = fixture(true);
    let runtime = rt.clone();
    let result = registry
        .dispatch_intercepted_with_metadata_with_identity(
            "search",
            &json!({"kind":"entity", "query":"emptyneedle"}),
            None,
            |_namespace| {
                // Spawn during FnOnce construction, before it returns its future.
                let handle = tokio::spawn(khive_runtime::search_audit::inherit(async move {
                    let token = runtime.authorize(Namespace::local()).unwrap();
                    let hits = runtime
                        .hybrid_search(&token, "emptyneedle", None, 10, None, None, &[], None)
                        .await?;
                    assert!(hits.is_empty());
                    Ok::<_, khive_runtime::RuntimeError>(InterceptedDispatchResult::new(
                        json!([]),
                        (),
                    ))
                }));
                async move { handle.await.unwrap() }
            },
        )
        .await
        .unwrap();
    assert_eq!(result.result, json!([]));
    let event = audit(&rt, "search").await;
    assert_eq!(event.payload["arms"], "both");
    assert_eq!(event.payload["result_count"], 0);
}
