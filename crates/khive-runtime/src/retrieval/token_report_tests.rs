use super::*;
use crate::embedder_registry::EmbedderProvider;
use lattice_embed::{EmbedError, EmbeddingsWithReport};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug)]
enum Behavior {
    Report,
    Unsupported,
    Failure,
    MissingVector,
    ExtraVector,
    MissingCount,
    ExtraCount,
    InvalidCount,
}

#[derive(Debug, PartialEq)]
struct Call {
    reported: bool,
    model: EmbeddingModel,
    texts: Vec<String>,
}

struct ReportingService {
    behavior: Behavior,
    calls: Arc<Mutex<Vec<Call>>>,
}

#[async_trait::async_trait]
impl EmbeddingService for ReportingService {
    async fn embed(&self, _: &[String], _: EmbeddingModel) -> lattice_embed::Result<Vec<Vec<f32>>> {
        panic!("document embeddings must preserve the passage entrypoint")
    }

    async fn embed_passage(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> lattice_embed::Result<Vec<Vec<f32>>> {
        self.calls.lock().unwrap().push(Call {
            reported: false,
            model,
            texts: texts.to_vec(),
        });
        Ok(texts.iter().map(|text| vec![text.len() as f32]).collect())
    }

    async fn embed_with_report(
        &self,
        texts: &[String],
        model: EmbeddingModel,
        role: EmbeddingRole,
    ) -> lattice_embed::Result<EmbeddingsWithReport> {
        assert_eq!(role, EmbeddingRole::Passage);
        self.calls.lock().unwrap().push(Call {
            reported: true,
            model,
            texts: texts.to_vec(),
        });
        match self.behavior {
            Behavior::Unsupported => return Err(EmbedError::Unsupported("no token report".into())),
            Behavior::Failure => return Err(EmbedError::InferenceFailed("report failure".into())),
            _ => {}
        }
        let mut vectors: Vec<_> = texts.iter().map(|text| vec![text.len() as f32]).collect();
        let mut counts: Vec<_> = texts
            .iter()
            .map(|text| {
                let before = text.chars().count() + 2;
                TokenCount::new(before, before.min(8))
            })
            .collect();
        match self.behavior {
            Behavior::MissingVector => {
                vectors.pop();
            }
            Behavior::ExtraVector => vectors.push(vec![0.0]),
            Behavior::MissingCount => {
                counts.pop();
            }
            Behavior::ExtraCount => counts.push(TokenCount::new(2, 2)),
            Behavior::InvalidCount => counts[0] = TokenCount::new(2, 3),
            _ => {}
        }
        Ok(EmbeddingsWithReport::new(vectors, counts))
    }

    fn supports_model(&self, _: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "report-test"
    }
}

struct ReportingProvider(Arc<ReportingService>);

#[async_trait::async_trait]
impl EmbedderProvider for ReportingProvider {
    fn name(&self) -> &str {
        "report-test"
    }
    fn dimensions(&self) -> usize {
        1
    }
    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(self.0.clone())
    }
}

fn fixture(behavior: Behavior) -> (KhiveRuntime, Arc<Mutex<Vec<Call>>>) {
    let runtime = KhiveRuntime::memory().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    runtime.register_embedder(ReportingProvider(Arc::new(ReportingService {
        behavior,
        calls: calls.clone(),
    })));
    (runtime, calls)
}

#[tokio::test]
async fn token_only_truncation_is_disclosed_below_the_byte_limit() {
    let (runtime, calls) = fixture(Behavior::Report);
    let text = "abcdefghijk";
    let outcome = runtime
        .embed_document_with_model_outcome("report-test", text)
        .await
        .unwrap();
    assert_eq!(outcome.vector, vec![11.0]);
    assert_eq!(outcome.source_bytes, text.len());
    assert_eq!(outcome.embedded_bytes, text.len());
    assert_eq!(outcome.provider_token_count, Some(TokenCount::new(13, 8)));
    assert!(outcome.truncated);
    assert_eq!(outcome.prepared_text_fingerprint, None);
    let mut report = EmbeddingTruncationReport::default();
    report.observe(&outcome);
    assert_eq!(
        report,
        EmbeddingTruncationReport {
            truncated: 1,
            discarded_bytes: 0,
            discarded_tokens: 5,
            token_reporting_unavailable: 0,
        }
    );
    assert_eq!(
        *calls.lock().unwrap(),
        vec![Call {
            reported: true,
            model: EmbeddingModel::default(),
            texts: vec![text.into()],
        }]
    );
    let mut response = serde_json::json!({"id":"preserved"});
    crate::embedding_warning::add_embedding_truncation_warning(&mut response, &report);
    assert_eq!(
        response["warnings"],
        serde_json::json!([EMBEDDING_INPUT_TRUNCATED_WARNING])
    );
    assert_eq!(response["id"], "preserved");
}

#[tokio::test]
async fn mixed_token_reports_preserve_batch_order_and_count_each_cut_once() {
    let (runtime, calls) = fixture(Behavior::Report);
    let texts = vec![
        "a".into(),
        "abcdefghijk".into(),
        "x".repeat(MAX_TEXT_BYTES + 5),
    ];
    let outcomes = runtime
        .embed_document_batch_with_model_outcomes("report-test", &texts)
        .await
        .unwrap();
    assert_eq!(outcomes.len(), 3);
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.truncated)
            .collect::<Vec<_>>(),
        vec![false, true, true]
    );
    let mut report = EmbeddingTruncationReport::default();
    for (text, outcome) in texts.iter().zip(&outcomes) {
        let bounded = text.len().min(MAX_TEXT_BYTES);
        assert_eq!(outcome.vector, vec![bounded as f32]);
        assert_eq!(outcome.source_bytes, text.len());
        assert_eq!(outcome.embedded_bytes, bounded);
        assert_eq!(
            outcome.provider_token_count,
            Some(TokenCount::new(bounded + 2, (bounded + 2).min(8)))
        );
        assert_eq!(outcome.prepared_text_fingerprint, None);
        report.observe(outcome);
    }
    assert_eq!(report.truncated, 2);
    assert_eq!(report.discarded_bytes, 5);
    assert_eq!(report.discarded_tokens, 5 + MAX_TEXT_BYTES as u64 + 2 - 8);
    assert_eq!(report.token_reporting_unavailable, 0);
    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "one ordered batch, not singleton inference"
    );
    assert_eq!(calls.lock().unwrap()[0].texts[2].len(), MAX_TEXT_BYTES);
    for (text, batch_outcome) in texts.iter().zip(outcomes) {
        let single = runtime
            .embed_document_with_model_outcome("report-test", text)
            .await
            .unwrap();
        assert_eq!(single.vector, batch_outcome.vector);
        assert_eq!(
            single.provider_token_count,
            batch_outcome.provider_token_count
        );
        assert_eq!(single.truncated, batch_outcome.truncated);
    }
}

#[tokio::test]
async fn unsupported_reporting_preserves_passage_fallback_and_unknown_counts() {
    let (runtime, calls) = fixture(Behavior::Unsupported);
    let texts = vec!["short".into(), "x".repeat(MAX_TEXT_BYTES + 3)];
    let outcomes = runtime
        .embed_document_batch_with_model_outcomes("report-test", &texts)
        .await
        .unwrap();
    let mut report = EmbeddingTruncationReport::default();
    for outcome in &outcomes {
        assert_eq!(outcome.provider_token_count, None);
        report.observe(outcome);
    }
    assert_eq!(outcomes[0].vector, vec![5.0]);
    assert!(!outcomes[0].truncated);
    assert!(outcomes[1].truncated);
    assert_eq!(
        report,
        EmbeddingTruncationReport {
            truncated: 1,
            discarded_bytes: 3,
            discarded_tokens: 0,
            token_reporting_unavailable: 2,
        }
    );
    let captured = calls.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert!(captured[0].reported);
    assert!(!captured[1].reported);
    assert_eq!(captured[0].texts, captured[1].texts);
    assert_eq!(captured[0].model, captured[1].model);
}

#[tokio::test]
async fn reporting_errors_never_retry_through_vector_only_embedding() {
    for batch in [false, true] {
        let (runtime, calls) = fixture(Behavior::Failure);
        let error = if batch {
            runtime
                .embed_document_batch_with_model_outcomes("report-test", &["sample".into()])
                .await
                .unwrap_err()
        } else {
            runtime
                .embed_document_with_model_outcome("report-test", "sample")
                .await
                .unwrap_err()
        };
        assert!(error.to_string().contains("report failure"));
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert!(calls.lock().unwrap()[0].reported);
    }
}

#[tokio::test]
async fn malformed_provider_reports_fail_single_and_batch_before_returning_vectors() {
    for behavior in [
        Behavior::MissingVector,
        Behavior::ExtraVector,
        Behavior::MissingCount,
        Behavior::ExtraCount,
        Behavior::InvalidCount,
    ] {
        for batch in [false, true] {
            let (runtime, calls) = fixture(behavior);
            let error = if batch {
                runtime
                    .embed_document_batch_with_model_outcomes(
                        "report-test",
                        &["one".into(), "two".into()],
                    )
                    .await
                    .unwrap_err()
            } else {
                runtime
                    .embed_document_with_model_outcome("report-test", "one")
                    .await
                    .unwrap_err()
            };
            assert!(
                error.to_string().contains("embed_with_report returned"),
                "{behavior:?}: {error}"
            );
            assert_eq!(calls.lock().unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn vector_only_batch_refuses_token_truncation_and_empty_batch_skips_provider() {
    let (runtime, calls) = fixture(Behavior::Report);
    assert!(runtime
        .embed_document_batch_with_model_outcomes("report-test", &[])
        .await
        .unwrap()
        .is_empty());
    assert!(calls.lock().unwrap().is_empty());
    let error = runtime
        .embed_document_batch_with_model("report-test", &["abcdefghijk".into()])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("5 discarded tokens"));
}

#[test]
fn old_aggregate_payloads_default_new_counters_and_merge_saturates() {
    let old: EmbeddingTruncationReport =
        serde_json::from_str(r#"{"truncated":2,"discarded_bytes":3}"#).unwrap();
    assert_eq!(
        old,
        EmbeddingTruncationReport {
            truncated: 2,
            discarded_bytes: 3,
            ..Default::default()
        }
    );
    assert_eq!(
        serde_json::to_value(&old).unwrap(),
        serde_json::json!({"truncated":2,"discarded_bytes":3})
    );
    let mut report = EmbeddingTruncationReport {
        truncated: u64::MAX,
        discarded_bytes: u64::MAX,
        discarded_tokens: u64::MAX,
        token_reporting_unavailable: u64::MAX,
    };
    report.merge(EmbeddingTruncationReport {
        truncated: 1,
        discarded_bytes: 1,
        discarded_tokens: 1,
        token_reporting_unavailable: 1,
    });
    assert_eq!(
        report,
        EmbeddingTruncationReport {
            truncated: u64::MAX,
            discarded_bytes: u64::MAX,
            discarded_tokens: u64::MAX,
            token_reporting_unavailable: u64::MAX,
        }
    );
    assert_eq!(
        serde_json::from_value::<EmbeddingTruncationReport>(serde_json::to_value(&report).unwrap())
            .unwrap(),
        report
    );
}

#[tokio::test]
async fn document_vector_refusal_describes_actual_truncation_cause() {
    let (reported, _) = fixture(Behavior::Report);
    let token_only = reported
        .embed_document_with_model_outcome("report-test", "abcdefghijk")
        .await
        .unwrap();
    assert_eq!(token_only.source_bytes, token_only.embedded_bytes);
    let token_error = document_vector_or_refusal(token_only).unwrap_err();
    assert!(matches!(&token_error, RuntimeError::InvalidInput(_)));
    assert!(token_error
        .to_string()
        .contains("from 13 to 8 provider tokens"));
    assert!(!token_error.to_string().contains(" bytes"));

    let large = "x".repeat(MAX_TEXT_BYTES + 3);
    let combined = reported
        .embed_document_with_model_outcome("report-test", &large)
        .await
        .unwrap();
    let combined_error = document_vector_or_refusal(combined).unwrap_err();
    assert!(matches!(&combined_error, RuntimeError::InvalidInput(_)));
    let combined_message = combined_error.to_string();
    assert!(combined_message.contains(&format!(
        "from {} to {} bytes",
        large.len(),
        MAX_TEXT_BYTES
    )));
    assert!(combined_message.contains(&format!(
        "from {} to 8 provider tokens for the byte-bounded input",
        MAX_TEXT_BYTES + 2
    )));

    let (unsupported, _) = fixture(Behavior::Unsupported);
    let byte_only = unsupported
        .embed_document_with_model_outcome("report-test", &large)
        .await
        .unwrap();
    assert!(byte_only.provider_token_count.is_none());
    let byte_error = document_vector_or_refusal(byte_only).unwrap_err();
    assert!(matches!(&byte_error, RuntimeError::InvalidInput(_)));
    let RuntimeError::InvalidInput(byte_message) = byte_error else {
        panic!("expected an input refusal");
    };
    assert_eq!(byte_message, format!(
        "embedding input truncated from {} to {} bytes; use embed_document_outcome to inspect the bounded vector",
        large.len(), MAX_TEXT_BYTES
    ));

    for runtime in [&reported, &unsupported] {
        let complete = runtime
            .embed_document_with_model_outcome("report-test", "short")
            .await
            .unwrap();
        assert!(!complete.truncated);
        assert_eq!(document_vector_or_refusal(complete).unwrap(), vec![5.0]);
    }
}

struct DefaultReportingProvider {
    name: String,
    dimensions: usize,
    service: Arc<ReportingService>,
}

#[async_trait::async_trait]
impl EmbedderProvider for DefaultReportingProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn dimensions(&self) -> usize {
        self.dimensions
    }
    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(self.service.clone())
    }
}

fn default_fixture(behavior: Behavior) -> (KhiveRuntime, Arc<Mutex<Vec<Call>>>) {
    let model = EmbeddingModel::AllMiniLmL6V2;
    let runtime = KhiveRuntime::new(crate::RuntimeConfig {
        db_path: None,
        embedding_model: Some(model),
        packs: vec!["kg".into()],
        ..crate::RuntimeConfig::no_embeddings()
    })
    .expect("in-memory default-model runtime");
    let calls = Arc::new(Mutex::new(Vec::new()));
    runtime.register_embedder(DefaultReportingProvider {
        name: model.to_string(),
        dimensions: model.dimensions(),
        service: Arc::new(ReportingService {
            behavior,
            calls: calls.clone(),
        }),
    });
    (runtime, calls)
}

#[tokio::test]
async fn embed_document_wrapper_reports_token_byte_and_combined_refusals() {
    let model_name = EmbeddingModel::AllMiniLmL6V2.to_string();
    let budget = document_embedding_budget(&model_name);
    let large = "x".repeat(budget + 3);
    let (reported, reported_calls) = default_fixture(Behavior::Report);
    let complete = reported.embed_document("short").await;
    assert_eq!(
        reported_calls.lock().unwrap().len(),
        1,
        "default provider was called"
    );
    assert_eq!(complete.unwrap(), vec![5.0]);

    let token_only = reported.embed_document("abcdefghijk").await;
    assert_eq!(
        reported_calls.lock().unwrap().len(),
        2,
        "token-only provider call"
    );
    let RuntimeError::InvalidInput(token_message) = token_only.unwrap_err() else {
        panic!("token-only input must be refused as InvalidInput");
    };
    assert!(token_message.contains("from 13 to 8 provider tokens"));
    assert!(!token_message.contains(" bytes"));

    let combined = reported.embed_document(&large).await;
    assert_eq!(
        reported_calls.lock().unwrap().len(),
        3,
        "combined provider call"
    );
    let RuntimeError::InvalidInput(combined_message) = combined.unwrap_err() else {
        panic!("combined truncation must be refused as InvalidInput");
    };
    assert!(combined_message.contains(&format!("from {} to {} bytes", large.len(), budget)));
    assert!(combined_message.contains(&format!(
        "from {} to 8 provider tokens for the byte-bounded input",
        budget + 2
    )));
    assert_eq!(reported_calls.lock().unwrap()[2].texts[0].len(), budget);

    let (unsupported, unsupported_calls) = default_fixture(Behavior::Unsupported);
    let complete = unsupported.embed_document("short").await;
    assert_eq!(
        unsupported_calls.lock().unwrap().len(),
        2,
        "report then passage fallback"
    );
    assert_eq!(complete.unwrap(), vec![5.0]);
    let bytes = unsupported.embed_document(&large).await;
    assert_eq!(
        unsupported_calls.lock().unwrap().len(),
        4,
        "byte-only report then fallback"
    );
    let RuntimeError::InvalidInput(byte_message) = bytes.unwrap_err() else {
        panic!("byte-only truncation must be refused as InvalidInput");
    };
    assert_eq!(byte_message, format!(
        "embedding input truncated from {} to {} bytes; use embed_document_outcome to inspect the bounded vector",
        large.len(), budget
    ));
    assert_eq!(unsupported_calls.lock().unwrap()[3].texts[0].len(), budget);
}
