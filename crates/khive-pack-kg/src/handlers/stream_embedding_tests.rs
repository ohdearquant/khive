use super::*;
use khive_runtime::{EmbedderProvider, Namespace, RuntimeConfig};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct CountingEmbedding {
    dimensions: usize,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl EmbeddingService for CountingEmbedding {
    async fn embed(
        &self,
        texts: &[String],
        _: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![vec![0.5; self.dimensions]; texts.len()])
    }
    fn supports_model(&self, _: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "stream-embedding-test"
    }
}

struct CountingProvider {
    name: String,
    dimensions: usize,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl EmbedderProvider for CountingProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn dimensions(&self) -> usize {
        self.dimensions
    }
    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
        Ok(Arc::new(CountingEmbedding {
            dimensions: self.dimensions,
            calls: self.calls.clone(),
        }))
    }
}

async fn embedding_surface() -> (KhiveRuntime, VerbRegistry, Arc<AtomicUsize>) {
    let model = EmbeddingModel::AllMiniLmL6V2;
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string()],
        brain_profile: None,
        actor_id: None,
        embedding_model: Some(model),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    for (name, dimensions) in [
        (model.to_string(), model.dimensions()),
        ("stream-secondary".into(), 4),
    ] {
        rt.register_embedder(CountingProvider {
            name,
            dimensions,
            calls: calls.clone(),
        });
    }
    let token = rt.authorize(Namespace::local()).unwrap();
    assert_eq!(rt.registered_embedding_model_names().len(), 2);
    for name in rt.registered_embedding_model_names() {
        assert_eq!(
            rt.vectors_for_model(&token, &name)
                .unwrap()
                .count()
                .await
                .unwrap(),
            0
        );
    }
    let mut builder = VerbRegistryBuilder::new();
    builder.register(crate::KgPack::new(rt.clone()));
    (rt, builder.build().unwrap(), calls)
}

async fn embedding_counts(rt: &KhiveRuntime) -> Vec<u64> {
    let token = rt.authorize(Namespace::local()).unwrap();
    let mut names = rt.registered_embedding_model_names();
    names.sort();
    let mut counts = Vec::new();
    for name in names {
        counts.push(
            rt.vectors_for_model(&token, &name)
                .unwrap()
                .count()
                .await
                .unwrap(),
        );
    }
    counts
}

async fn vector_queue(rt: &KhiveRuntime) -> i64 {
    let value = rt
        .sql()
        .reader()
        .await
        .unwrap()
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM ann_write_log".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    let Some(SqlValue::Integer(count)) = value else {
        panic!("integer queue count")
    };
    count
}

async fn append_with_embedding(
    registry: &VerbRegistry,
    atomic: Option<bool>,
    fields: Value,
) -> Value {
    let mut args = json!({"stream": "embedding", "record": "orchid ledger"});
    args.as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    if let Some(atomic) = atomic {
        args["op"] = json!("append");
        registry
            .dispatch("stream.batch", json!({"atomic": atomic, "ops": [args]}))
            .await
            .unwrap()["results"][0]
            .clone()
    } else {
        registry.dispatch("stream.append", args).await.unwrap()
    }
}

#[tokio::test]
async fn stream_append_embedding_arm1_default_and_explicit_controls() {
    let (rt, registry, calls) = embedding_surface().await;
    for fields in [json!({}), json!({"embed": false})] {
        append_with_embedding(&registry, None, fields).await;
        assert_eq!(embedding_counts(&rt).await, vec![0, 0]);
        assert_eq!(vector_queue(&rt).await, 0);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    append_with_embedding(&registry, None, json!({"embed": true})).await;
    assert_eq!(embedding_counts(&rt).await, vec![1, 1]);
    assert_eq!(vector_queue(&rt).await, 2);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn stream_batch_embedding_arm1_both_modes() {
    for atomic in [true, false] {
        let (rt, registry, calls) = embedding_surface().await;
        for fields in [json!({}), json!({"embed": false})] {
            append_with_embedding(&registry, Some(atomic), fields).await;
            assert_eq!(embedding_counts(&rt).await, vec![0, 0]);
            assert_eq!(vector_queue(&rt).await, 0);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
        append_with_embedding(&registry, Some(atomic), json!({"embed": true})).await;
        assert_eq!(embedding_counts(&rt).await, vec![1, 1]);
        assert_eq!(vector_queue(&rt).await, 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn stream_embedding_arm2_similarity_and_lexical_controls() {
    for atomic in [None, Some(true), Some(false)] {
        let (_, registry, _) = embedding_surface().await;
        let off = append_with_embedding(&registry, atomic, json!({})).await;
        let on = append_with_embedding(&registry, atomic, json!({"embed": true})).await;
        // Exact-source "both" requires a vector contribution for this own-content query.
        let hits = registry
            .dispatch(
                "search",
                json!({"kind":"observation", "query":"\"orchid ledger\"", "source":"both"}),
            )
            .await
            .unwrap();
        let hits = hits.as_array().unwrap();
        assert!(!hits.iter().any(|hit| hit["id"] == off["id"]));
        assert!(hits.iter().any(|hit| hit["id"] == on["id"]));
        let lexical = registry
            .dispatch(
                "search",
                json!({"kind":"observation", "query":"\"orchid ledger\"", "source":"text"}),
            )
            .await
            .unwrap();
        assert!(lexical
            .as_array()
            .unwrap()
            .iter()
            .any(|hit| hit["id"] == off["id"]));
        let listed = registry
            .dispatch("list", json!({"kind":"observation"}))
            .await
            .unwrap();
        assert!(listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hit| hit["id"] == off["id"]));
    }
}

#[tokio::test]
async fn stream_embedding_model_selects_only_one_registered_model() {
    for atomic in [None, Some(true), Some(false)] {
        let (rt, registry, calls) = embedding_surface().await;
        append_with_embedding(
            &registry,
            atomic,
            json!({"embed":true, "embedding_model":EmbeddingModel::AllMiniLmL6V2.to_string()}),
        )
        .await;
        assert_eq!(embedding_counts(&rt).await, vec![1, 0]);
        assert_eq!(vector_queue(&rt).await, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

fn invalid_model_error(error: RuntimeError, member: Option<&str>) {
    let RuntimeError::Khive(error) = error else {
        panic!("structured invalid input: {error:?}")
    };
    let value = serde_json::to_value(error).unwrap();
    assert_eq!(value["kind"], "invalid_input");
    if let Some(member) = member {
        assert_eq!(value["details"]["member"], member);
    }
}

#[tokio::test]
async fn stream_append_embedding_model_requires_explicit_true() {
    let (rt, registry, calls) = embedding_surface().await;
    let before = population(&rt).await;
    for embed in [None, Some(false)] {
        let mut args = json!({"stream":"invalid", "record":null, "embedding_model":EmbeddingModel::AllMiniLmL6V2.to_string()});
        if let Some(embed) = embed {
            args["embed"] = json!(embed);
        }
        invalid_model_error(
            registry.dispatch("stream.append", args).await.unwrap_err(),
            None,
        );
        assert_eq!(population(&rt).await, before);
        assert_eq!(heads(&registry, &["invalid"]).await, vec![0]);
        assert_eq!(vector_queue(&rt).await, 0);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn stream_batch_embedding_model_refuses_whole_list_before_writes() {
    for atomic in [true, false] {
        let (rt, registry, calls) = embedding_surface().await;
        let before = population(&rt).await;
        let before_schema = schema(&rt).await;
        for embed in [None, Some(false)] {
            let mut bad = json!({"op":"append", "stream":"bad", "record":null, "embedding_model":EmbeddingModel::AllMiniLmL6V2.to_string()});
            if let Some(embed) = embed {
                bad["embed"] = json!(embed);
            }
            let error = registry
                .dispatch(
                    "stream.batch",
                    json!({"atomic":atomic, "ops":[
                        {"op":"append", "stream":"good", "record":1, "embed":true}, bad
                    ]}),
                )
                .await
                .unwrap_err();
            invalid_model_error(error, atomic.then_some("1"));
            assert_eq!(population(&rt).await, before);
            assert_eq!(schema(&rt).await, before_schema);
            assert_eq!(heads(&registry, &["good", "bad"]).await, vec![0, 0]);
            assert_eq!(vector_queue(&rt).await, 0);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn stream_embedding_unknown_fields_and_model_are_refused() {
    let (rt, registry, calls) = embedding_surface().await;
    let before = population(&rt).await;
    for atomic in [None, Some(true), Some(false)] {
        for fields in [
            json!({"embedd":false}),
            json!({"embed":true,"embedding_model":"missing-model"}),
        ] {
            let mut args = json!({"stream":"invalid", "record":null});
            args.as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            let error = if let Some(atomic) = atomic {
                args["op"] = json!("append");
                registry
                    .dispatch("stream.batch", json!({"atomic":atomic,"ops":[args]}))
                    .await
                    .unwrap_err()
            } else {
                registry.dispatch("stream.append", args).await.unwrap_err()
            };
            assert!(
                matches!(
                    error,
                    RuntimeError::InvalidInput(_) | RuntimeError::UnknownModel(_)
                ),
                "{error:?}"
            );
            assert_eq!(population(&rt).await, before);
            assert_eq!(vector_queue(&rt).await, 0);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn stream_embedding_help_exposes_defaults_and_model_requirement() {
    let (_, registry) = surface();
    let help = registry
        .dispatch("stream.append", json!({"help":true}))
        .await
        .unwrap();
    let params = help["params"].as_array().unwrap();
    for (name, expected) in [
        ("embed", "Defaults false"),
        ("embedding_model", "Requires embed=true"),
    ] {
        let param = params.iter().find(|param| param["name"] == name).unwrap();
        assert!(param["description"].as_str().unwrap().contains(expected));
    }
    let help = registry
        .dispatch("stream.batch", json!({"help":true}))
        .await
        .unwrap();
    let ops = help["params"]
        .as_array()
        .unwrap()
        .iter()
        .find(|param| param["name"] == "ops")
        .unwrap();
    assert!(ops["description"]
        .as_str()
        .unwrap()
        .contains("embed defaults false"));
    assert!(ops["description"]
        .as_str()
        .unwrap()
        .contains("embedding_model requires embed=true"));
}
