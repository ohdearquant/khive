use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{Event, EventFilter, PageRequest, SqlStatement, SqlValue};
use khive_types::{EventKind, Id128, RerankExecutedPayload, RerankerKind};
use serde_json::{json, Value};

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
}
impl Fixture {
    fn new() -> Self {
        let config = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            visible_namespaces: Vec::new(),
            allowed_outbound_namespaces: Vec::new(),
            embedding_model: None,
            additional_embedding_models: Vec::new(),
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            disk_guard_environment: Default::default(),
            disk_guard_config: None,
            volume_lock_dir: None,
            credentials: Vec::new(),
            visibility_receipts: None,
            actor_id: None,
            brain_profile: None,
            brain: Default::default(),
            blob: Default::default(),
            packs: vec!["kg".into(), "memory".into()],
            mounts: Vec::new(),
            events_split: None,
            ..RuntimeConfig::no_embeddings()
        };
        let runtime = KhiveRuntime::new(config).unwrap();
        assert!(runtime.backend().pool().canonical_path().is_none());
        assert!(runtime.default_embedder_name().is_empty());
        let mut builder = VerbRegistryBuilder::new();
        builder.with_default_namespace("local");
        builder.with_actor_id(Some("rerank-caller".into()));
        builder.register(KgPack::new(runtime.clone()));
        builder.register(MemoryPack::new_with_index_role(runtime.clone(), false));
        Self {
            runtime,
            registry: builder.build().unwrap(),
        }
    }
    async fn events(&self, namespace: &str) -> Vec<Event> {
        let token = self
            .runtime
            .authorize(Namespace::parse(namespace).unwrap())
            .unwrap();
        self.runtime
            .events(&token)
            .unwrap()
            .query_events(
                EventFilter {
                    kinds: vec![EventKind::RerankExecuted],
                    ..Default::default()
                },
                PageRequest {
                    limit: 100,
                    offset: 0,
                },
            )
            .await
            .unwrap()
            .items
    }
    async fn observations(&self, event: &Event) -> Vec<(String, String, i64, String)> {
        let access = self.runtime.sql();
        let mut reader = access.reader().await.unwrap();
        reader.query_all(SqlStatement {
            sql: "SELECT role, entity_id, position, referent_kind FROM event_observations WHERE event_id = ?1 ORDER BY role, position".into(),
            params: vec![SqlValue::Text(event.id.to_string())], label: Some("test.rerank.projection".into()),
        }).await.unwrap().into_iter().map(|row| (
            row.text("role").unwrap().into(), row.text("entity_id").unwrap().into(),
            row.i64("position").unwrap(), row.text("referent_kind").unwrap().into(),
        )).collect()
    }
}

#[tokio::test]
async fn weighted_event_records_actual_features_tiers_namespace_and_output_order() {
    let f = Fixture::new();
    let a = "00000000-0000-4000-8000-000000000001";
    let b = "00000000-0000-4000-8000-000000000002";
    let query = " query/session-7 ";
    let response = f.registry.dispatch("memory.recall_rerank", json!({
        "namespace": "rerank-test", "query_id": query,
        "candidates": [
            {"id": a, "fused_score": 0.125, "salience": 0.75, "age_days": 0, "temporal": 0.25, "source": "both"},
            {"id": b, "fused_score": 0.875, "salience": 0.5, "age_days": 0, "temporal": 0.5, "source": "vector"}
        ],
        "config": {"reranker_weights": {"temporal": 1.0, "relevance": 2.0, "text_match": 0.0, "typo": 7.0, "zero_unknown": 0.0}}
    })).await.unwrap();
    assert_eq!(response["reranked"][0]["id"], a);
    assert_eq!(response["reranked"][1]["id"], b);
    let first_score = response["reranked"][0]["rerank_score"].as_f64().unwrap();
    assert_eq!(first_score, 1.0 / 6.0);
    assert_eq!(response["reranked"][0]["rerank_scores"]["relevance"], 0.25);
    assert_eq!(response["reranked"][1]["rerank_score"], 0.75);
    let mut active: Vec<&str> = response["active_rerankers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    active.sort_unstable();
    assert_eq!(active, ["relevance", "temporal", "typo"]);
    let events = f.events("rerank-test").await;
    assert_eq!(events.len(), 1);
    assert!(f.events("local").await.is_empty());
    let event = &events[0];
    assert_eq!(event.namespace, "rerank-test");
    assert_eq!(event.actor, "rerank-caller");
    assert_eq!(event.verb, "memory.recall_rerank");
    assert!(event.payload.get("model_id").is_none());
    let payload: RerankExecutedPayload = serde_json::from_value(event.payload.clone()).unwrap();
    assert_eq!(payload.reranker, RerankerKind::Weighted);
    assert_eq!(payload.query_id.as_deref(), Some(query));
    assert_eq!(payload.model_id, None);
    assert_eq!(payload.served_by_profile_id, None);
    assert!(!payload.hook_applied && !payload.hook_target_match);
    assert_eq!(payload.tiers, vec!["relevance", "temporal"]);
    assert_eq!(payload.ignored_weights, vec!["typo", "zero_unknown"]);
    assert_eq!(payload.unidentified_candidates, 0);
    assert_eq!(
        payload.candidates,
        vec![a.parse::<Id128>().unwrap(), b.parse().unwrap()]
    );
    assert_eq!(
        payload.reranked[0].1,
        vec![
            ("relevance".into(), 0.125),
            ("salience".into(), 0.75),
            ("temporal".into(), 0.25),
            ("text_match".into(), 1.0),
            ("vector_match".into(), 1.0)
        ]
    );
    assert_eq!(
        payload.final_scores,
        vec![
            (a.parse().unwrap(), first_score as f32),
            (b.parse().unwrap(), 0.75)
        ]
    );
    assert_ne!(
        f64::from(payload.final_scores[0].1),
        first_score,
        "event narrowing must not leak back into the f64 response"
    );
    assert_eq!(event.duration_us, payload.latency_us as i64);
    assert_eq!(
        f.observations(event).await,
        vec![
            ("candidate".into(), a.into(), 0, "note".into()),
            ("candidate".into(), b.into(), 1, "note".into()),
            ("selected".into(), a.into(), 0, "note".into()),
            ("selected".into(), b.into(), 1, "note".into()),
        ]
    );
}

#[tokio::test]
async fn unidentified_candidates_are_returned_but_not_given_event_identities() {
    let f = Fixture::new();
    let id = "ABCDEF00-0000-4000-8000-000000000001";
    let response = f.registry.dispatch("memory.recall_rerank", json!({
        "candidates": [{"id": id}, {}, {"id": "malformed"}, {"id": 7}, {"id": null}, {"id": id}],
        "config": {"reranker_weights": {"relevance": 1.0}}
    })).await.unwrap();
    let returned = response["reranked"].as_array().unwrap();
    assert_eq!(returned.len(), 6);
    assert_eq!(
        returned.iter().map(|v| v["id"].clone()).collect::<Vec<_>>(),
        vec![
            json!(id),
            Value::Null,
            json!("malformed"),
            json!(7),
            Value::Null,
            json!(id)
        ]
    );
    let events = f.events("local").await;
    assert_eq!(events.len(), 1);
    assert!(events[0].payload.get("query_id").is_none());
    let payload: RerankExecutedPayload = serde_json::from_value(events[0].payload.clone()).unwrap();
    let canonical = id.parse::<Id128>().unwrap();
    assert_eq!(payload.candidates, vec![canonical, canonical]);
    assert_eq!(payload.unidentified_candidates, 4);
    assert_eq!(payload.reranked.len(), 2);
    assert_eq!(
        payload.final_scores,
        vec![(canonical, 0.0), (canonical, 0.0)]
    );
    assert_eq!(f.observations(&events[0]).await.len(), 4);
}

#[tokio::test]
async fn empty_and_no_tier_calls_each_emit_one_event_without_changing_response_shape() {
    let f = Fixture::new();
    assert_eq!(
        f.registry
            .dispatch(
                "memory.recall_rerank",
                json!({"candidates": [], "query_id": null})
            )
            .await
            .unwrap(),
        json!({"reranked": [], "active_rerankers": []})
    );
    let id = "00000000-0000-4000-8000-000000000009";
    assert_eq!(
        f.registry
            .dispatch(
                "memory.recall_rerank",
                json!({"candidates": [{"id": id, "fused_score": 0.75}]})
            )
            .await
            .unwrap(),
        json!({"reranked": [{"id": id, "rerank_score": 0.0, "rerank_scores": {}}], "active_rerankers": []})
    );
    let events = f.events("local").await;
    assert_eq!(events.len(), 2);
    for event in events {
        let payload: RerankExecutedPayload = serde_json::from_value(event.payload).unwrap();
        assert!(payload.tiers.is_empty());
        assert!(payload.ignored_weights.is_empty());
        assert_eq!(payload.query_id, None);
        assert_eq!(payload.model_id, None);
        assert!(payload.final_scores.iter().all(|(_, s)| *s == 0.0));
    }
}

#[tokio::test]
async fn invalid_config_and_query_types_emit_no_rerank_event() {
    let f = Fixture::new();
    for params in [
        json!({"candidates": [], "config": {"reranker_weights": {"relevance": -1.0}}}),
        json!({"candidates": [], "query_id": 7}),
        json!({"candidates": [], "query_id": {"id": "query"}}),
    ] {
        assert!(f
            .registry
            .dispatch("memory.recall_rerank", params)
            .await
            .is_err());
    }
    assert!(f.events("local").await.is_empty());
}

#[tokio::test]
async fn finite_narrowing_keeps_f64_response_and_allows_underflow_to_zero() {
    let f = Fixture::new();
    let ids = [
        "00000000-0000-4000-8000-000000000011",
        "00000000-0000-4000-8000-000000000012",
        "00000000-0000-4000-8000-000000000013",
        "00000000-0000-4000-8000-000000000014",
    ];
    // The next f64 above f32::MAX still rounds to a finite f32; this is
    // narrowing, not a pre-cast range rejection or score clamping policy.
    let just_above = f64::from_bits(f64::from(f32::MAX).to_bits() + 1);
    let scores = [
        f64::from(f32::MAX),
        -f64::from(f32::MAX),
        f64::from_bits(1),
        just_above,
    ];
    let candidates: Vec<Value> = ids
        .iter()
        .zip(scores)
        .map(|(id, score)| json!({"id": id, "fused_score": score}))
        .collect();
    let response = f
        .registry
        .dispatch(
            "memory.recall_rerank",
            json!({"candidates": candidates, "config": {"reranker_weights": {"relevance": 1.0}}}),
        )
        .await
        .unwrap();
    for (row, score) in response["reranked"].as_array().unwrap().iter().zip(scores) {
        assert_eq!(row["rerank_score"].as_f64().unwrap(), score);
    }
    let events = f.events("local").await;
    assert_eq!(events.len(), 1);
    let payload: RerankExecutedPayload = serde_json::from_value(events[0].payload.clone()).unwrap();
    assert_eq!(
        payload
            .final_scores
            .iter()
            .map(|(_, score)| *score)
            .collect::<Vec<_>>(),
        vec![f32::MAX, -f32::MAX, 0.0, f32::MAX]
    );
}

#[tokio::test]
async fn score_overflow_keeps_candidate_identity_and_response_but_omits_score_tuples() {
    let f = Fixture::new();
    let a = "00000000-0000-4000-8000-000000000021";
    let b = "00000000-0000-4000-8000-000000000022";
    let c = "00000000-0000-4000-8000-000000000023";
    let response = f.registry.dispatch("memory.recall_rerank", json!({
        "candidates": [{"id": a, "fused_score": 1e300}, {"id": b, "fused_score": 0.5}, {"id": c, "fused_score": -1e300}],
        "config": {"reranker_weights": {"relevance": 1.0}}
    })).await.unwrap();
    assert_eq!(
        response["reranked"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["rerank_score"].as_f64().unwrap())
            .collect::<Vec<_>>(),
        vec![1e300, 0.5, -1e300]
    );
    let events = f.events("local").await;
    assert_eq!(events.len(), 1);
    let payload: RerankExecutedPayload = serde_json::from_value(events[0].payload.clone()).unwrap();
    assert_eq!(
        payload.candidates,
        [a, b, c].map(|id| id.parse::<Id128>().unwrap())
    );
    assert_eq!(payload.unidentified_candidates, 0);
    assert_eq!(payload.final_scores, vec![(b.parse().unwrap(), 0.5)]);
    assert_eq!(
        payload
            .reranked
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        vec![b.parse::<Id128>().unwrap()]
    );
    assert!(payload.is_valid());
    assert_eq!(
        f.observations(&events[0]).await,
        vec![
            ("candidate".into(), a.into(), 0, "note".into()),
            ("candidate".into(), b.into(), 1, "note".into()),
            ("candidate".into(), c.into(), 2, "note".into()),
            ("selected".into(), b.into(), 0, "note".into()),
        ]
    );
}

#[tokio::test]
async fn failed_event_append_does_not_replace_the_rerank_response() {
    let f = Fixture::new();
    {
        let sql = f.runtime.sql();
        let mut writer = sql.writer().await.unwrap();
        writer.execute(SqlStatement {
            sql: "CREATE TRIGGER reject_rerank_event BEFORE INSERT ON events WHEN NEW.kind = 'rerank_executed' BEGIN SELECT RAISE(ABORT, 'test rerank append failure'); END".into(),
            params: vec![], label: Some("test.rerank.append_failure".into()),
        }).await.unwrap();
    }
    let id = "00000000-0000-4000-8000-000000000031";
    let response = f
        .registry
        .dispatch(
            "memory.recall_rerank",
            json!({
                "candidates": [{"id": id, "fused_score": 0.25}],
                "config": {"reranker_weights": {"relevance": 1.0}}
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        response,
        json!({"reranked": [{"id": id, "rerank_score": 0.25, "rerank_scores": {"relevance": 0.25}}], "active_rerankers": ["relevance"]})
    );
    assert!(f.events("local").await.is_empty());
}

#[tokio::test]
async fn unrepresentable_raw_features_omit_both_tuples_even_when_final_score_is_finite() {
    let f = Fixture::new();
    let ids = [
        "00000000-0000-4000-8000-000000000041",
        "00000000-0000-4000-8000-000000000042",
        "00000000-0000-4000-8000-000000000043",
    ];
    let response = f
        .registry
        .dispatch(
            "memory.recall_rerank",
            json!({
                "candidates": [
                    {"id": ids[0], "fused_score": 0.5, "salience": 1e300},
                    {"id": ids[1], "fused_score": 0.5, "salience": 1.0, "age_days": -1e300},
                    {"id": ids[2], "fused_score": 0.25}
                ],
                "config": {"reranker_weights": {"relevance": 1.0}}
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        response["reranked"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["rerank_score"].as_f64().unwrap())
            .collect::<Vec<_>>(),
        vec![0.5, 0.5, 0.25]
    );
    let events = f.events("local").await;
    assert_eq!(events.len(), 1);
    let payload: RerankExecutedPayload = serde_json::from_value(events[0].payload.clone()).unwrap();
    assert_eq!(
        payload.candidates,
        ids.map(|id| id.parse::<Id128>().unwrap())
    );
    assert_eq!(payload.unidentified_candidates, 0);
    assert_eq!(payload.final_scores, vec![(ids[2].parse().unwrap(), 0.25)]);
    assert_eq!(
        payload
            .reranked
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        vec![ids[2].parse::<Id128>().unwrap()]
    );
    assert!(payload.is_valid());
}
