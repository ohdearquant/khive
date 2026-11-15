#![cfg(feature = "native-rerank")]

use std::path::Path;
use std::sync::Arc;

use khive_retrieval::{
    CrossEncoderScorer, NativeCrossEncoderModel, NativeCrossEncoderReranker,
    RerankDocumentResolver, Reranker, RetrievalError,
};
use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, RuntimeConfig, RuntimeRerankDocumentResolver,
};
use khive_score::DeterministicScore;
use khive_storage::Entity;
use serde_json::json;
use uuid::Uuid;

const HIDDEN: usize = 4;
const QUERY: &str = "query";
const DOCUMENTS: [&str; 2] = ["document bright", "document dark"];

type NativeReranker =
    NativeCrossEncoderReranker<Uuid, RuntimeRerankDocumentResolver, NativeCrossEncoderModel>;
type Tensor = (String, Vec<usize>, Vec<f32>);

fn tensor(name: impl Into<String>, shape: &[usize], values: &[f32]) -> Tensor {
    assert_eq!(shape.iter().product::<usize>(), values.len());
    (name.into(), shape.to_vec(), values.to_vec())
}

fn checkpoint(classifier: Option<([f32; HIDDEN], f32)>) -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("vocab.txt"),
        "[PAD]\n[UNK]\n[CLS]\n[SEP]\n[MASK]\nquery\nother\nbright\ndark\ndocument\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("config.json"),
        json!({
            "vocab_size": 10,
            "hidden_size": HIDDEN,
            "num_hidden_layers": 1,
            "num_attention_heads": 1,
            "intermediate_size": HIDDEN,
            "max_position_embeddings": 32,
            "type_vocab_size": 2,
            "layer_norm_eps": 1e-5,
        })
        .to_string(),
    )
    .unwrap();

    let mut embeddings = vec![0.0; 10 * HIDDEN];
    for (token, vector) in [
        (5, [1.0, -1.0, 0.0, 0.0]),
        (6, [0.0, 1.0, -1.0, 0.0]),
        (7, [2.0, 0.0, -1.0, -1.0]),
        (8, [-2.0, 0.0, 1.0, 1.0]),
    ] {
        embeddings[token * HIDDEN..(token + 1) * HIDDEN].copy_from_slice(&vector);
    }
    let mut tensors = vec![
        tensor(
            "embeddings.word_embeddings.weight",
            &[10, HIDDEN],
            &embeddings,
        ),
        tensor(
            "embeddings.position_embeddings.weight",
            &[32, HIDDEN],
            &[0.0; 32 * HIDDEN],
        ),
        tensor(
            "embeddings.token_type_embeddings.weight",
            &[2, HIDDEN],
            &[0.0; 2 * HIDDEN],
        ),
        tensor("embeddings.LayerNorm.weight", &[HIDDEN], &[1.0; HIDDEN]),
        tensor("embeddings.LayerNorm.bias", &[HIDDEN], &[0.0; HIDDEN]),
    ];
    if let Some((weights, bias)) = classifier {
        tensors.push(tensor("classifier.weight", &[1, HIDDEN], &weights));
        tensors.push(tensor("classifier.bias", &[1], &[bias]));
    }
    let mut identity = [0.0; HIDDEN * HIDDEN];
    for index in 0..HIDDEN {
        identity[index * HIDDEN + index] = 1.0;
    }
    // Zero Q/K makes attention uniform. Identity V/output and nonzero norm
    // scales carry different token directions into CLS, rather than fixing it.
    for module in [
        "attention.self.query",
        "attention.self.key",
        "attention.self.value",
        "attention.output.dense",
        "intermediate.dense",
        "output.dense",
    ] {
        let weights = if matches!(module, "attention.self.value" | "attention.output.dense") {
            identity
        } else {
            [0.0; HIDDEN * HIDDEN]
        };
        tensors.push(tensor(
            format!("encoder.layer.0.{module}.weight"),
            &[HIDDEN, HIDDEN],
            &weights,
        ));
        tensors.push(tensor(
            format!("encoder.layer.0.{module}.bias"),
            &[HIDDEN],
            &[0.0; HIDDEN],
        ));
    }
    for norm in ["attention.output.LayerNorm", "output.LayerNorm"] {
        tensors.push(tensor(
            format!("encoder.layer.0.{norm}.weight"),
            &[HIDDEN],
            &[1.0; HIDDEN],
        ));
        tensors.push(tensor(
            format!("encoder.layer.0.{norm}.bias"),
            &[HIDDEN],
            &[0.0; HIDDEN],
        ));
    }
    write_safetensors(directory.path(), tensors);
    directory
}

fn write_safetensors(directory: &Path, tensors: Vec<Tensor>) {
    let mut header = serde_json::Map::new();
    let mut payload = Vec::new();
    for (name, shape, values) in tensors {
        let start = payload.len();
        payload.extend(values.iter().flat_map(|value| value.to_le_bytes()));
        header.insert(
            name,
            json!({"dtype": "F32", "shape": shape, "data_offsets": [start, payload.len()]}),
        );
    }
    let mut header = serde_json::to_vec(&header).unwrap();
    header.resize(header.len().next_multiple_of(8), b' ');
    let mut bytes = (header.len() as u64).to_le_bytes().to_vec();
    bytes.extend(header);
    bytes.extend(payload);
    std::fs::write(directory.join("model.safetensors"), bytes).unwrap();
}

fn input_sensitive_checkpoint() -> tempfile::TempDir {
    checkpoint(Some(([0.8, 0.0, 0.0, 0.0], 0.0)))
}

fn assert_input_sensitive(model: &NativeCrossEncoderModel) -> [f32; 2] {
    let scores = DOCUMENTS.map(|document| model.score(QUERY, document));
    for score in scores {
        assert!(score.is_finite() && (0.0..=1.0).contains(&score));
    }
    assert!(
        (scores[0] - scores[1]).abs() > 0.01,
        "native fixture must distinguish the stored descriptions: {scores:?}"
    );
    assert!(
        (scores[0] - model.score("other", DOCUMENTS[0])).abs() > 0.01,
        "native fixture must depend on the query"
    );
    let name_only = model.score(QUERY, "document");
    assert!(
        scores.iter().all(|score| (score - name_only).abs() > 0.01),
        "name-only resolution must produce different scores"
    );
    scores
}

#[test]
fn native_model_scores_match_ordered_single_pairs() {
    let directory = input_sensitive_checkpoint();
    let model = NativeCrossEncoderModel::from_directory(directory.path()).unwrap();
    assert_input_sensitive(&model);
    for documents in [
        vec![DOCUMENTS[0], DOCUMENTS[1]],
        vec![DOCUMENTS[1], DOCUMENTS[0], DOCUMENTS[1], ""],
        vec![],
    ] {
        for query in [QUERY, "other"] {
            let actual = CrossEncoderScorer::score_batch(&model, query, &documents);
            let expected: Vec<_> = documents
                .iter()
                .map(|document| model.score(query, document).to_bits())
                .collect();
            assert_eq!(
                actual
                    .iter()
                    .map(|score| score.to_bits())
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }
}

#[test]
fn native_model_returns_sigmoid_probabilities() {
    let directory = checkpoint(Some(([0.0; HIDDEN], 1.25)));
    let model = NativeCrossEncoderModel::from_directory(directory.path()).unwrap();
    let expected = 1.0 / (1.0 + (-1.25_f32).exp());
    let actual = CrossEncoderScorer::score_batch(&model, QUERY, &DOCUMENTS);
    assert_eq!(actual.len(), 2);
    for score in actual {
        assert!((score - expected).abs() < 1e-6, "{score} != {expected}");
        assert!((0.0..=1.0).contains(&score));
    }
}

fn runtime() -> (Arc<KhiveRuntime>, NamespaceToken) {
    let runtime = Arc::new(
        KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap(),
    );
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.backend_ann_root().is_none());
    assert!(runtime.registered_embedding_model_names().is_empty());
    let token = runtime.authorize(Namespace::local()).unwrap();
    (runtime, token)
}

async fn create_entity(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    name: &str,
    description: Option<&str>,
) -> Entity {
    runtime
        .create_entity_with_embedding_report(
            token,
            "concept",
            None,
            name,
            description,
            None,
            vec![],
        )
        .await
        .unwrap()
        .0
}

#[tokio::test]
async fn runtime_resolver_preserves_order_and_entity_text() {
    let (runtime, token) = runtime();
    let described = create_entity(&runtime, &token, "document", Some("bright")).await;
    let plain = create_entity(&runtime, &token, "plain", None).await;
    let empty_description = create_entity(&runtime, &token, "empty", Some("")).await;
    let deleted = create_entity(&runtime, &token, "deleted", Some("dark")).await;
    runtime
        .delete_entity(&token, deleted.id, false)
        .await
        .unwrap();
    let foreign_token = runtime
        .authorize(Namespace::parse("other").unwrap())
        .unwrap();
    let foreign = create_entity(&runtime, &foreign_token, "foreign", Some("dark")).await;
    let blank = Entity::new("local", "concept", "");
    runtime
        .entities(&token)
        .unwrap()
        .upsert_entity(blank.clone())
        .await
        .unwrap();
    let missing = Uuid::new_v4();
    let resolver = RuntimeRerankDocumentResolver::new(runtime.clone(), token.clone());
    assert_eq!(
        resolver
            .resolve_documents(&[
                foreign.id,
                described.id,
                missing,
                plain.id,
                described.id,
                empty_description.id,
                deleted.id,
                blank.id,
            ])
            .await
            .unwrap(),
        vec![
            Some("foreign dark".into()),
            Some("document bright".into()),
            None,
            Some("plain".into()),
            Some("document bright".into()),
            Some("empty".into()),
            None,
            Some(String::new()),
        ]
    );
    assert!(resolver.resolve_documents(&[]).await.unwrap().is_empty());
}

#[tokio::test]
async fn native_model_rejects_invalid_checkpoint() {
    let directory = checkpoint(None);
    let (runtime, token) = runtime();
    let resolver = Arc::new(RuntimeRerankDocumentResolver::new(runtime, token));
    match NativeReranker::from_directory(directory.path(), resolver) {
        Err(RetrievalError::Rerank(message)) => assert!(!message.is_empty()),
        Err(other) => panic!("load failure changed its error kind: {other:?}"),
        Ok(_) => panic!("checkpoint without a classifier must be refused"),
    }
}

fn candidates(ids: &[Uuid]) -> Vec<(Uuid, DeterministicScore)> {
    ids.iter()
        .map(|id| (*id, DeterministicScore::from_f64(0.01)))
        .collect()
}

#[tokio::test]
async fn runtime_native_reranker_scores_stored_entities() {
    let directory = input_sensitive_checkpoint();
    let model = Arc::new(NativeCrossEncoderModel::from_directory(directory.path()).unwrap());
    let scores = assert_input_sensitive(&model);
    let (runtime, token) = runtime();
    let bright = create_entity(&runtime, &token, "document", Some("bright")).await;
    let dark = create_entity(&runtime, &token, "document", Some("dark")).await;
    let entries = [
        (
            bright.id,
            DeterministicScore::from_f64(f64::from(scores[0])),
        ),
        (dark.id, DeterministicScore::from_f64(f64::from(scores[1]))),
    ];
    assert_ne!(entries[0].1, entries[1].1);
    let (high, low) = if scores[0] > scores[1] {
        (entries[0], entries[1])
    } else {
        (entries[1], entries[0])
    };
    let reranker = runtime.native_cross_encoder_reranker(token.clone(), model);
    let expected = vec![high, low];
    assert_eq!(
        reranker
            .rerank(QUERY, candidates(&[low.0, high.0]), 2)
            .await
            .unwrap(),
        expected
    );
    let missing = Uuid::new_v4();
    assert_eq!(
        reranker
            .rerank(QUERY, candidates(&[low.0, missing, high.0]), 3)
            .await
            .unwrap(),
        expected
    );
    assert_eq!(
        reranker
            .rerank(QUERY, candidates(&[missing, low.0, high.0]), 1)
            .await
            .unwrap(),
        vec![high]
    );
    assert_eq!(
        reranker
            .rerank(QUERY, candidates(&[low.0, high.0, low.0]), 3)
            .await
            .unwrap(),
        vec![high, low, low]
    );
    let loaded = NativeReranker::from_directory(
        directory.path(),
        Arc::new(RuntimeRerankDocumentResolver::new(runtime, token)),
    )
    .unwrap();
    assert_eq!(
        loaded
            .rerank(QUERY, candidates(&[low.0, high.0]), 2)
            .await
            .unwrap(),
        expected
    );
}
