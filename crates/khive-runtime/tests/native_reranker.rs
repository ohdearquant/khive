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

use khive_retrieval::{LoraHook, LoraTargetGeometry, NoopLoraHook};
use std::sync::atomic::{AtomicUsize, Ordering};

struct DormantHook {
    active_layer: Option<usize>,
    calls: AtomicUsize,
}

impl LoraHook for DormantHook {
    fn is_active(&self, layer: usize, module: &str) -> bool {
        self.active_layer == Some(layer) && module == "ffn_output"
    }

    fn apply(&self, _: usize, _: &str, _: &[f32], _: &mut [f32]) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

struct ProjectionDelta<'a> {
    calls: &'a AtomicUsize,
}

impl LoraHook for ProjectionDelta<'_> {
    fn is_active(&self, layer: usize, module: &str) -> bool {
        layer == 0 && module == "ffn_output"
    }

    fn apply(&self, layer: usize, module: &str, x: &[f32], output: &mut [f32]) {
        assert_eq!(layer, 0);
        assert_eq!(module, "ffn_output");
        assert_eq!(x.len(), HIDDEN);
        assert_eq!(output.len(), HIDDEN);
        assert!(x.iter().all(|value| value.is_finite()));
        self.calls.fetch_add(1, Ordering::SeqCst);
        // A uniform delta would disappear at the following layer normalization.
        output[0] += 4.0;
        output[1] -= 2.0;
    }
}

fn score_bits(scores: &[f32]) -> Vec<u32> {
    scores.iter().map(|score| score.to_bits()).collect()
}

#[test]
fn native_hook_identity_paths_preserve_exact_scores_and_provenance() {
    let directory = input_sensitive_checkpoint();
    let model = NativeCrossEncoderModel::from_directory(directory.path()).unwrap();
    let baseline = assert_input_sensitive(&model);
    let inactive = DormantHook {
        active_layer: None,
        calls: AtomicUsize::new(0),
    };
    let unreachable = DormantHook {
        active_layer: Some(99),
        calls: AtomicUsize::new(0),
    };
    assert!(unreachable.is_active(99, "ffn_output"));
    for hook in [
        None,
        Some(&NoopLoraHook as &dyn LoraHook),
        Some(&inactive),
        Some(&unreachable),
    ] {
        let direct =
            CrossEncoderScorer::score_batch_with_hook(&model, QUERY, &DOCUMENTS, hook).unwrap();
        assert_eq!(
            score_bits(&direct.scores),
            score_bits(&baseline),
            "identity hooks must preserve every native score bit"
        );
        assert!(
            !direct.hook_applied,
            "hook presence or unreachable activity is not native application"
        );
    }
    assert_eq!(inactive.calls.load(Ordering::SeqCst), 0);
    assert_eq!(unreachable.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn native_hook_changes_real_forward_scores_and_tracks_application() {
    let directory = input_sensitive_checkpoint();
    let model = Arc::new(NativeCrossEncoderModel::from_directory(directory.path()).unwrap());
    let baseline = assert_input_sensitive(&model);
    let calls = AtomicUsize::new(0);
    let hook = ProjectionDelta { calls: &calls };
    let adapted =
        CrossEncoderScorer::score_batch_with_hook(model.as_ref(), QUERY, &DOCUMENTS, Some(&hook))
            .unwrap();
    assert!(
        calls.load(Ordering::SeqCst) > 0,
        "native hook must reach the actual forward projection"
    );
    assert!(adapted.hook_applied, "actual native apply must be reported");
    assert_eq!(adapted.scores.len(), DOCUMENTS.len());
    assert!(adapted
        .scores
        .iter()
        .all(|score| score.is_finite() && (0.0..=1.0).contains(score)));
    assert_ne!(
        score_bits(&adapted.scores),
        score_bits(&baseline),
        "real projection adaptation must change the native scores"
    );
    let (runtime, token) = runtime();
    let bright = create_entity(&runtime, &token, "document", Some("bright")).await;
    let dark = create_entity(&runtime, &token, "document", Some("dark")).await;
    let input = candidates(&[dark.id, Uuid::new_v4(), bright.id, dark.id]);
    let reranker = runtime.native_cross_encoder_reranker(token, model.clone());
    let unadapted = reranker.rerank(QUERY, input.clone(), 3).await.unwrap();
    let mut expected = vec![
        (dark.id, adapted.scores[1]),
        (bright.id, adapted.scores[0]),
        (dark.id, adapted.scores[1]),
    ];
    expected.sort_by(|left, right| right.1.total_cmp(&left.1));
    let expected: Vec<_> = expected
        .into_iter()
        .map(|(id, score)| (id, DeterministicScore::from_f64(f64::from(score))))
        .collect();
    calls.store(0, Ordering::SeqCst);
    let result = reranker
        .rerank_with_hook(QUERY, input, 3, Some(&hook))
        .await
        .unwrap();
    assert!(
        calls.load(Ordering::SeqCst) > 0,
        "stored-document reranking must forward this invocation's borrowed hook"
    );
    assert!(result.hook_applied);
    assert_eq!(
        result.results, expected,
        "stored IDs and scores must match the actual adapted native pairs"
    );
    assert_ne!(result.results, unadapted);
    let direct =
        CrossEncoderScorer::score_batch_with_hook(model.as_ref(), QUERY, &DOCUMENTS, None).unwrap();
    assert_eq!(score_bits(&direct.scores), score_bits(&baseline));
    assert!(!direct.hook_applied);
}

struct RejectingGeometry {
    legacy: bool,
    calls: AtomicUsize,
    compatibility_checks: AtomicUsize,
    neutral_checks: AtomicUsize,
}

impl LoraHook for RejectingGeometry {
    fn apply(&self, _: usize, _: &str, _: &[f32], _: &mut [f32]) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }

    #[allow(deprecated)]
    fn validate_against_bert(
        &self,
        layers: usize,
        hidden: usize,
        intermediate: usize,
    ) -> std::result::Result<(), String> {
        assert_eq!((layers, hidden, intermediate), (1, HIDDEN, HIDDEN));
        self.compatibility_checks.fetch_add(1, Ordering::SeqCst);
        if self.legacy {
            Err("native hook geometry sentinel".into())
        } else {
            Ok(())
        }
    }

    fn validate_geometry(&self, _: &dyn LoraTargetGeometry) -> std::result::Result<(), String> {
        self.neutral_checks.fetch_add(1, Ordering::SeqCst);
        Err("native hook geometry sentinel".into())
    }
}

#[tokio::test]
async fn native_hook_geometry_refusal_precedes_forward() {
    let directory = input_sensitive_checkpoint();
    let model = Arc::new(NativeCrossEncoderModel::from_directory(directory.path()).unwrap());
    let (runtime, token) = runtime();
    let entity = create_entity(&runtime, &token, "document", Some("bright")).await;
    let reranker = runtime.native_cross_encoder_reranker(token, model.clone());
    for legacy in [false, true] {
        let hook = RejectingGeometry {
            legacy,
            calls: AtomicUsize::new(0),
            compatibility_checks: AtomicUsize::new(0),
            neutral_checks: AtomicUsize::new(0),
        };
        let validation_counts = || {
            (
                hook.compatibility_checks.load(Ordering::SeqCst),
                hook.neutral_checks.load(Ordering::SeqCst),
            )
        };
        let assert_validated = |(previous_compatibility, previous_neutral)| {
            let (compatibility, neutral) = validation_counts();
            assert!(compatibility > previous_compatibility);
            if legacy {
                assert_eq!(
                    neutral, previous_neutral,
                    "legacy refusal must short-circuit"
                );
            } else {
                assert!(neutral > previous_neutral);
            }
            assert_eq!(hook.calls.load(Ordering::SeqCst), 0);
        };
        for documents in [&[][..], &DOCUMENTS[..]] {
            let before = validation_counts();
            let result = CrossEncoderScorer::score_batch_with_hook(
                model.as_ref(),
                QUERY,
                documents,
                Some(&hook),
            );
            assert!(
                matches!(result, Err(RetrievalError::Rerank(ref message)) if message.contains("native hook geometry sentinel")),
                "native hook geometry refusal must survive even an empty scoring batch"
            );
            assert_validated(before);
        }
        let before = validation_counts();
        let result = reranker
            .rerank_with_hook(QUERY, candidates(&[entity.id]), 1, Some(&hook))
            .await;
        assert!(
            matches!(result, Err(RetrievalError::Rerank(ref message)) if message.contains("native hook geometry sentinel")),
            "stored-document reranking must preserve native geometry refusal"
        );
        assert_validated(before);
        let before_skips = validation_counts();
        for (input, limit) in [
            (vec![], 1),
            (candidates(&[entity.id]), 0),
            (candidates(&[Uuid::new_v4()]), 1),
        ] {
            let skipped = reranker
                .rerank_with_hook(QUERY, input, limit, Some(&hook))
                .await
                .unwrap();
            assert!(skipped.results.is_empty());
            assert!(!skipped.hook_applied);
        }
        assert_eq!(
            validation_counts(),
            before_skips,
            "existing early-empty reranker paths must skip scoring and validation"
        );
        assert_eq!(hook.calls.load(Ordering::SeqCst), 0);
    }
}
