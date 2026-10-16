use super::*;
use crate::{EmbedderProvider, Namespace, NoteEmbeddingPolicy, NoteEmbeddingPolicySpec};
use khive_storage::{TextFilter, TextQueryMode, TextSearch, TextSearchRequest};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::Barrier;

tokio::task_local! {
    static BEFORE_PUBLICATION: (&'static str, Arc<(Barrier, Barrier)>);
}

pub(super) async fn pause(stage: &str) {
    if let Ok(Some(barriers)) = BEFORE_PUBLICATION
        .try_with(|(selected, barriers)| (*selected == stage).then(|| Arc::clone(barriers)))
    {
        barriers.0.wait().await;
        barriers.1.wait().await;
    }
}

struct Provider {
    name: String,
    calls: Arc<AtomicUsize>,
    fail: bool,
    dimensions: usize,
}
struct Service {
    calls: Arc<AtomicUsize>,
    dimensions: usize,
}
#[async_trait::async_trait]
impl EmbedderProvider for Provider {
    fn name(&self) -> &str {
        &self.name
    }
    fn dimensions(&self) -> usize {
        self.dimensions
    }
    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        if self.fail {
            return Err(RuntimeError::Internal("fixture model unavailable".into()));
        }
        Ok(Arc::new(Service {
            calls: self.calls.clone(),
            dimensions: self.dimensions,
        }))
    }
}
#[async_trait::async_trait]
impl EmbeddingService for Service {
    async fn embed(
        &self,
        texts: &[String],
        _: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.calls.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts.iter().map(|_| vec![0.25; self.dimensions]).collect())
    }
    fn supports_model(&self, _: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "record-repair-fixture"
    }
}

fn model(runtime: &KhiveRuntime, name: &str, fail: bool) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    let dimensions = runtime.embedder_dimensions(name).unwrap_or(4);
    runtime.register_embedder(Provider {
        name: name.into(),
        calls: calls.clone(),
        fail,
        dimensions,
    });
    calls
}

async fn seed(runtime: &KhiveRuntime, token: &NamespaceToken, entity: bool) -> Record {
    if entity {
        Record::Entity(
            runtime
                .create_entity(
                    token,
                    "concept",
                    None,
                    "repairneedle",
                    Some("record recovery"),
                    Some(json!({"fixture":true})),
                    vec!["repair".into()],
                )
                .await
                .unwrap(),
        )
    } else {
        Record::Note(
            runtime
                .create_note(
                    token,
                    "observation",
                    Some("repairneedle"),
                    "record recovery",
                    None,
                    Some(json!({"fixture":true})),
                    vec![],
                )
                .await
                .unwrap(),
        )
    }
}

fn text(runtime: &KhiveRuntime, token: &NamespaceToken, entity: bool) -> Arc<dyn TextSearch> {
    if entity {
        runtime.text(token).unwrap()
    } else {
        runtime.text_for_notes(token).unwrap()
    }
}

async fn search(store: &dyn TextSearch, namespace: &str, query: &str) -> Vec<Uuid> {
    store
        .search(TextSearchRequest {
            query: query.into(),
            mode: TextQueryMode::Plain,
            filter: Some(TextFilter {
                namespaces: vec![namespace.into()],
                ..Default::default()
            }),
            top_k: 20,
            snippet_chars: 100,
        })
        .await
        .unwrap()
        .into_iter()
        .map(|hit| hit.subject_id)
        .collect()
}

async fn ann_count(runtime: &KhiveRuntime) -> i64 {
    match runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_scalar(
            SqlStatement::new("SELECT COUNT(*) FROM ann_write_log", vec![])
                .labelled("record-index-repair"),
        )
        .await
        .unwrap()
    {
        Some(SqlValue::Integer(count)) => count,
        _ => panic!("ANN log count must be an integer"),
    }
}

async fn install_vector(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    doc: &TextDocument,
    model: &str,
    value: f32,
) {
    runtime
        .vectors_for_model(token, model)
        .unwrap()
        .insert(
            doc.subject_id,
            doc.kind,
            &doc.namespace,
            if doc.kind == SubstrateKind::Entity {
                "entity.body"
            } else {
                "note.content"
            },
            vec![vec![value; runtime.embedder_dimensions(model).unwrap()]],
        )
        .await
        .unwrap();
}

async fn assert_record_unchanged(runtime: &KhiveRuntime, token: &NamespaceToken, record: &Record) {
    match record {
        Record::Entity(expected) => assert_eq!(
            serde_json::to_value(
                runtime
                    .entities(token)
                    .unwrap()
                    .get_entity(expected.id)
                    .await
                    .unwrap()
            )
            .unwrap(),
            serde_json::to_value(Some(expected)).unwrap()
        ),
        Record::Note(expected) => assert_eq!(
            runtime
                .notes(token)
                .unwrap()
                .get_note(expected.id)
                .await
                .unwrap()
                .as_ref(),
            Some(expected)
        ),
    }
}

#[tokio::test]
async fn blank_text_repairs_fts_without_vector_work() {
    for entity in [false, true] {
        for body in ["", " \t\n"] {
            for stale in [false, true] {
                let runtime = KhiveRuntime::memory().unwrap();
                let token = NamespaceToken::local();
                let other = seed(&runtime, &token, entity).await;
                let record = if entity {
                    let record = Entity::new("local", "concept", body).with_description(body);
                    runtime
                        .entities(&token)
                        .unwrap()
                        .upsert_entity(record.clone())
                        .await
                        .unwrap();
                    Record::Entity(record)
                } else {
                    let record = Note::new("local", "observation", body);
                    runtime
                        .notes(&token)
                        .unwrap()
                        .upsert_note(record.clone())
                        .await
                        .unwrap();
                    Record::Note(record)
                };
                let doc = record.document();
                assert!(doc.body.trim().is_empty());
                let store = text(&runtime, &token, entity);
                if stale {
                    let mut wrong = doc.clone();
                    wrong.body = "obsolete blank-record index".into();
                    store.upsert_document(wrong).await.unwrap();
                }
                let other_before = serde_json::to_value(
                    store
                        .get_document("local", other.document().subject_id)
                        .await
                        .unwrap(),
                )
                .unwrap();
                let healthy_calls = model(&runtime, "healthy", false);
                let missing_calls = model(&runtime, "missing", false);
                let unavailable_calls = model(&runtime, "unavailable", true);
                let mut selected = match &record {
                    Record::Entity(_) => runtime.registered_embedding_model_names(),
                    Record::Note(note) => runtime.embedding_models_for_note_kind(&note.kind),
                };
                selected.sort();
                assert_eq!(selected, vec!["healthy", "missing", "unavailable"]);
                install_vector(&runtime, &token, &doc, "healthy", 0.75).await;
                install_vector(&runtime, &token, &other.document(), "healthy", 0.9).await;
                let before = ann_count(&runtime).await;

                let report = runtime
                    .repair_record_indexes(&token, doc.subject_id)
                    .await
                    .unwrap();
                assert_eq!(report.repaired, vec!["fts"]);
                assert!(report.failures.is_empty(), "{:?}", report.failures);
                assert!(same_document(
                    &store
                        .get_document("local", doc.subject_id)
                        .await
                        .unwrap()
                        .unwrap(),
                    &doc
                ));
                let repeat = runtime
                    .repair_record_indexes(&token, doc.subject_id)
                    .await
                    .unwrap();
                assert!(repeat.repaired.is_empty() && repeat.failures.is_empty());
                assert_eq!(healthy_calls.load(Ordering::SeqCst), 0);
                assert_eq!(missing_calls.load(Ordering::SeqCst), 0);
                assert_eq!(unavailable_calls.load(Ordering::SeqCst), 0);
                assert_eq!(ann_count(&runtime).await, before);
                let kept = runtime
                    .vectors_for_model(&token, "healthy")
                    .unwrap()
                    .get_vectors(
                        &[doc.subject_id, other.document().subject_id],
                        "local",
                        record.tables().2,
                    )
                    .await
                    .unwrap();
                assert_eq!(kept[&doc.subject_id], vec![0.75; 4]);
                assert_eq!(kept[&other.document().subject_id], vec![0.9; 4]);
                assert_eq!(
                    serde_json::to_value(
                        store
                            .get_document("local", other.document().subject_id)
                            .await
                            .unwrap()
                    )
                    .unwrap(),
                    other_before
                );
                assert_record_unchanged(&runtime, &token, &record).await;
                assert_record_unchanged(&runtime, &token, &other).await;
            }
        }
    }
}

#[tokio::test]
async fn vector_disappearing_after_identity_check_reports_presence_failure_without_embedding() {
    for entity in [false, true] {
        for remove in [false, true] {
            let runtime = Arc::new(KhiveRuntime::memory().unwrap());
            let token = NamespaceToken::local();
            let record = seed(&runtime, &token, entity).await;
            let other = seed(&runtime, &token, entity).await;
            let doc = record.document();
            let calls = model(&runtime, "read-race", false);
            install_vector(&runtime, &token, &doc, "read-race", 0.75).await;
            install_vector(&runtime, &token, &other.document(), "read-race", 0.9).await;
            let vectors = runtime.vectors_for_model(&token, "read-race").unwrap();
            let barriers = Arc::new((Barrier::new(2), Barrier::new(2)));
            let repair_runtime = runtime.clone();
            let repair_token = token.clone();
            let id = doc.subject_id;
            let repair = tokio::spawn(BEFORE_PUBLICATION.scope(
                ("vector_read", barriers.clone()),
                async move {
                    repair_runtime
                        .repair_record_indexes(&repair_token, id)
                        .await
                },
            ));
            tokio::time::timeout(std::time::Duration::from_secs(10), barriers.0.wait())
                .await
                .expect("repair observed the healthy identity before reading its vector");
            let present = vectors
                .get_vectors(&[id], "local", record.tables().2)
                .await
                .unwrap();
            assert_eq!(present[&id], vec![0.75; 4]);
            if remove {
                assert!(vectors.delete(id).await.unwrap());
            }
            let before = ann_count(&runtime).await;
            barriers.1.wait().await;
            let report = repair.await.unwrap().unwrap();
            assert!(report.repaired.is_empty());
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(ann_count(&runtime).await, before);
            if remove {
                assert_eq!(report.failures.len(), 1);
                assert_eq!(report.failures[0].stage, "vector_presence");
                assert_eq!(
                    report.failures[0].error,
                    format!(
                        "read-race: vector identity was present for {id}, \
                         but its vector was not returned"
                    )
                );
            } else {
                assert!(report.failures.is_empty());
            }
            let kept = vectors
                .get_vectors(
                    &[id, other.document().subject_id],
                    "local",
                    record.tables().2,
                )
                .await
                .unwrap();
            if remove {
                assert!(!kept.contains_key(&id));
            } else {
                assert_eq!(kept[&id], vec![0.75; 4]);
            }
            assert_eq!(kept[&other.document().subject_id], vec![0.9; 4]);
            assert_record_unchanged(&runtime, &token, &record).await;
            assert_record_unchanged(&runtime, &token, &other).await;
            if remove {
                let retry = runtime.repair_record_indexes(&token, id).await.unwrap();
                assert_eq!(retry.repaired, vec!["vector:read-race"]);
                assert!(retry.failures.is_empty());
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                assert_eq!(ann_count(&runtime).await, before + 1);
                let repeat = runtime.repair_record_indexes(&token, id).await.unwrap();
                assert!(repeat.repaired.is_empty() && repeat.failures.is_empty());
                assert_eq!(calls.load(Ordering::SeqCst), 1);
            }
        }
    }
}

#[tokio::test]
async fn degraded_note_insert_recovers_keyword_search_without_substrate_rewrite() {
    let runtime = KhiveRuntime::memory().unwrap();
    let namespace = format!("repair-{}", Uuid::new_v4());
    let token = runtime
        .authorize(Namespace::parse(&namespace).unwrap())
        .unwrap();
    let store = runtime.text_for_notes(&token).unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = attempts.clone();
    {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        runtime.backend().pool().writer().unwrap().conn().authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(context.action, AuthAction::Insert { table_name } if table_name == "fts_notes_rowids") {
                observed.fetch_add(1, Ordering::SeqCst);
                Authorization::Deny
            } else { Authorization::Allow }
        })).unwrap();
    }
    let result = runtime
        .try_create_note(
            &token,
            "observation",
            None,
            "degradedneedle committed body",
            None,
        )
        .await;
    runtime
        .backend()
        .pool()
        .writer()
        .unwrap()
        .conn()
        .authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>)
        .unwrap();
    assert!(
        attempts.load(Ordering::SeqCst) > 0,
        "fixture must execute the denied FTS index write"
    );
    let id = match result {
        Ok(Some(note)) => note.id,
        Ok(None) => panic!("unique fixture must create a note"),
        Err(error) => {
            let RuntimeError::Khive(domain) = error.refusal_source() else {
                panic!("unexpected conditional insert failure: {error:?}");
            };
            let details = domain.details().expect("committed degradation details");
            assert_eq!(details.get("reason"), Some("post_commit_degraded"));
            assert_eq!(details.get("committed"), Some("true"));
            assert!(details
                .get("post_commit_degradations")
                .unwrap()
                .contains("fts_upsert"));
            details.get("record_id").unwrap().parse().unwrap()
        }
    };
    let note = runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .expect("conditional insert retains its committed note");
    assert!(search(store.as_ref(), &namespace, "degradedneedle")
        .await
        .is_empty());
    let report = runtime
        .repair_record_indexes(&token, note.id)
        .await
        .unwrap();
    assert_eq!(report.repaired, vec!["fts"]);
    assert!(report.failures.is_empty());
    assert_eq!(
        search(store.as_ref(), &namespace, "degradedneedle").await,
        vec![note.id]
    );
    assert_eq!(
        runtime
            .notes(&token)
            .unwrap()
            .get_note(note.id)
            .await
            .unwrap()
            .unwrap(),
        note
    );
}

#[tokio::test]
async fn missing_and_stale_fts_repair_only_the_named_entity_or_note() {
    for entity in [false, true] {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = NamespaceToken::local();
        let record = seed(&runtime, &token, entity).await;
        let other = seed(&runtime, &token, entity).await;
        let doc = record.document();
        let other_doc = other.document();
        let store = text(&runtime, &token, entity);
        let source_before = if entity {
            serde_json::to_value(runtime.get_entity(&token, doc.subject_id).await.unwrap()).unwrap()
        } else {
            serde_json::to_value(
                runtime
                    .notes(&token)
                    .unwrap()
                    .get_note(doc.subject_id)
                    .await
                    .unwrap(),
            )
            .unwrap()
        };
        for stale in [false, true] {
            store
                .delete_document(&doc.namespace, doc.subject_id)
                .await
                .unwrap();
            if stale {
                let mut wrong = doc.clone();
                wrong.title = Some("obsolete title".into());
                wrong.body = "obsoletebody".into();
                wrong.record_kind = Some("wrong".into());
                store.upsert_document(wrong).await.unwrap();
            }
            let before_other = serde_json::to_value(
                store
                    .get_document(&other_doc.namespace, other_doc.subject_id)
                    .await
                    .unwrap(),
            )
            .unwrap();
            let report = runtime
                .repair_record_indexes(&token, doc.subject_id)
                .await
                .unwrap();
            assert_eq!(report.repaired, vec!["fts"]);
            assert!(report.failures.is_empty());
            assert!(same_document(
                &store
                    .get_document(&doc.namespace, doc.subject_id)
                    .await
                    .unwrap()
                    .unwrap(),
                &doc
            ));
            let found = search(store.as_ref(), &doc.namespace, "repairneedle").await;
            assert!(found.contains(&doc.subject_id));
            assert!(found.contains(&other_doc.subject_id));
            assert_eq!(
                serde_json::to_value(
                    store
                        .get_document(&other_doc.namespace, other_doc.subject_id)
                        .await
                        .unwrap()
                )
                .unwrap(),
                before_other
            );
            let report = runtime
                .repair_record_indexes(&token, doc.subject_id)
                .await
                .unwrap();
            assert!(
                report.repaired.is_empty() && report.failures.is_empty(),
                "healthy repeat must do no repair"
            );
        }
        let source_after = if entity {
            serde_json::to_value(runtime.get_entity(&token, doc.subject_id).await.unwrap()).unwrap()
        } else {
            serde_json::to_value(
                runtime
                    .notes(&token)
                    .unwrap()
                    .get_note(doc.subject_id)
                    .await
                    .unwrap(),
            )
            .unwrap()
        };
        assert_eq!(source_after, source_before);
    }
}

#[tokio::test]
async fn missing_vectors_preserve_healthy_models_other_records_and_ann_history() {
    for entity in [false, true] {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = NamespaceToken::local();
        let record = seed(&runtime, &token, entity).await;
        let other = seed(&runtime, &token, entity).await;
        let doc = record.document();
        let healthy_calls = model(&runtime, "healthy", false);
        let missing_calls = model(&runtime, "missing", false);
        install_vector(&runtime, &token, &doc, "healthy", 0.75).await;
        install_vector(&runtime, &token, &other.document(), "missing", 0.9).await;
        let before = ann_count(&runtime).await;
        let report = runtime
            .repair_record_indexes(&token, doc.subject_id)
            .await
            .unwrap();
        assert_eq!(report.repaired, vec!["vector:missing"]);
        assert!(report.failures.is_empty());
        assert_eq!(
            healthy_calls.load(Ordering::SeqCst),
            0,
            "healthy vector must not invoke its embedder"
        );
        assert_eq!(missing_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            ann_count(&runtime).await,
            before + 1,
            "only the missing vector publishes an ANN upsert"
        );
        let kept = runtime
            .vectors_for_model(&token, "healthy")
            .unwrap()
            .get_vectors(&[doc.subject_id], &doc.namespace, record.tables().2)
            .await
            .unwrap();
        assert_eq!(kept[&doc.subject_id], vec![0.75; 4]);
        let kept = runtime
            .vectors_for_model(&token, "missing")
            .unwrap()
            .get_vectors(
                &[other.document().subject_id],
                &doc.namespace,
                record.tables().2,
            )
            .await
            .unwrap();
        assert_eq!(kept[&other.document().subject_id], vec![0.9; 4]);
        let repeat = runtime
            .repair_record_indexes(&token, doc.subject_id)
            .await
            .unwrap();
        assert!(repeat.repaired.is_empty() && repeat.failures.is_empty());
        assert_eq!(missing_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ann_count(&runtime).await, before + 1);
    }
}

#[tokio::test]
async fn note_kind_selection_preserves_excluded_vectors_without_embedding_them() {
    let runtime = KhiveRuntime::new(crate::RuntimeConfig {
        db_path: None,
        brain_profile: None,
        embedding_model: Some(EmbeddingModel::default()),
        ..crate::RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let token = NamespaceToken::local();
    let primary = runtime.default_embedder_name().to_owned();
    let primary_calls = model(&runtime, &primary, false);
    let note = Note::new("local", "observation", "kind selected repair");
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    let record = Record::Note(note);
    let doc = record.document();
    let excluded_calls = model(&runtime, "excluded", false);
    runtime.install_note_embedding_policies(&[NoteEmbeddingPolicySpec {
        kind: "observation",
        policy: NoteEmbeddingPolicy::DefaultModel,
    }]);
    install_vector(&runtime, &token, &doc, "excluded", 0.8).await;
    let before = ann_count(&runtime).await;
    let report = runtime
        .repair_record_indexes(&token, doc.subject_id)
        .await
        .unwrap();
    assert_eq!(
        report.repaired,
        vec!["fts".to_string(), format!("vector:{primary}")]
    );
    assert!(report.failures.is_empty());
    assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
    assert_eq!(excluded_calls.load(Ordering::SeqCst), 0);
    let kept = runtime
        .vectors_for_model(&token, "excluded")
        .unwrap()
        .get_vectors(&[doc.subject_id], &doc.namespace, "note.content")
        .await
        .unwrap();
    assert_eq!(kept[&doc.subject_id], vec![0.8; 4]);
    assert_eq!(ann_count(&runtime).await, before + 1);
}

#[tokio::test]
async fn note_kind_selection_never_embeds_a_model_the_kind_excludes() {
    let runtime = KhiveRuntime::new(crate::RuntimeConfig {
        db_path: None,
        brain_profile: None,
        embedding_model: Some(EmbeddingModel::default()),
        ..crate::RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let token = NamespaceToken::local();
    let primary = runtime.default_embedder_name().to_owned();
    let primary_calls = model(&runtime, &primary, false);
    let note = Note::new("local", "observation", "kind selected repair");
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    let record = Record::Note(note);
    let doc = record.document();
    let excluded_calls = model(&runtime, "excluded", false);
    runtime.install_note_embedding_policies(&[NoteEmbeddingPolicySpec {
        kind: "observation",
        policy: NoteEmbeddingPolicy::DefaultModel,
    }]);
    // The excluded model holds a vector for another note only, so nothing
    // healthy can be skipped for this one: only the kind's own model
    // selection keeps the repair from embedding it.
    let other = Note::new("local", "observation", "another observation");
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(other.clone())
        .await
        .unwrap();
    install_vector(
        &runtime,
        &token,
        &Record::Note(other).document(),
        "excluded",
        0.5,
    )
    .await;
    let report = runtime
        .repair_record_indexes(&token, doc.subject_id)
        .await
        .unwrap();
    assert_eq!(
        report.repaired,
        vec!["fts".to_string(), format!("vector:{primary}")]
    );
    assert!(report.failures.is_empty());
    assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
    assert_eq!(excluded_calls.load(Ordering::SeqCst), 0);
    let excluded = runtime
        .vectors_for_model(&token, "excluded")
        .unwrap()
        .get_vectors(&[doc.subject_id], &doc.namespace, "note.content")
        .await
        .unwrap();
    assert!(!excluded.contains_key(&doc.subject_id));
}

#[tokio::test]
async fn failures_report_prior_fts_repair_and_continue_other_missing_models() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let record = seed(&runtime, &token, false).await;
    let doc = record.document();
    runtime
        .text_for_notes(&token)
        .unwrap()
        .delete_document(&doc.namespace, doc.subject_id)
        .await
        .unwrap();
    model(&runtime, "a-broken", true);
    model(&runtime, "z-healthy", false);
    let report = runtime
        .repair_record_indexes(&token, doc.subject_id)
        .await
        .unwrap();
    assert_eq!(report.repaired, vec!["fts", "vector:z-healthy"]);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].stage, "embedding");
    assert!(report.failures[0].error.contains("a-broken"));
    let value = serde_json::to_value(report).unwrap();
    assert_eq!(value["substrate"], "note");
    assert_eq!(value["failures"][0]["stage"], "embedding");
    assert!(search(
        runtime.text_for_notes(&token).unwrap().as_ref(),
        "local",
        "repairneedle"
    )
    .await
    .contains(&doc.subject_id));
}

#[tokio::test]
async fn missing_or_deleted_id_refuses_and_does_not_repair_another_record() {
    for entity in [false, true] {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = NamespaceToken::local();
        let record = seed(&runtime, &token, entity).await;
        let other = seed(&runtime, &token, entity).await;
        let doc = record.document();
        let other_doc = other.document();
        let store = text(&runtime, &token, entity);
        store
            .delete_document(&other_doc.namespace, other_doc.subject_id)
            .await
            .unwrap();
        if entity {
            runtime
                .delete_entity(&token, doc.subject_id, false)
                .await
                .unwrap();
        } else {
            runtime
                .delete_note(&token, doc.subject_id, false)
                .await
                .unwrap();
        }
        for id in [Uuid::new_v4(), doc.subject_id] {
            assert!(matches!(
                runtime.repair_record_indexes(&token, id).await,
                Err(RuntimeError::NotFound(_))
            ));
        }
        assert!(store
            .get_document(&other_doc.namespace, other_doc.subject_id)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .get_document(&doc.namespace, doc.subject_id)
            .await
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn selected_namespace_refuses_foreign_ids_without_index_writes() {
    for entity in [false, true] {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = runtime
            .authorize(Namespace::parse("repair-owner").unwrap())
            .unwrap();
        let record = seed(&runtime, &token, entity).await;
        let doc = record.document();
        let store = text(&runtime, &token, entity);
        store
            .delete_document(&doc.namespace, doc.subject_id)
            .await
            .unwrap();
        let calls = model(&runtime, "foreign", false);
        let before = ann_count(&runtime).await;
        assert!(matches!(
            runtime
                .repair_record_indexes(&NamespaceToken::local(), doc.subject_id)
                .await,
            Err(RuntimeError::NotFound(_))
        ));
        assert!(store
            .get_document("local", doc.subject_id)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .get_document("repair-owner", doc.subject_id)
            .await
            .unwrap()
            .is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(ann_count(&runtime).await, before);
    }
}

#[tokio::test]
async fn shared_entity_note_uuid_is_ambiguous_before_any_index_work() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let entity = seed(&runtime, &token, true).await;
    let doc = entity.document();
    runtime
        .text(&token)
        .unwrap()
        .delete_document("local", doc.subject_id)
        .await
        .unwrap();
    let mut note = Note::new("local", "observation", "shared UUID fixture");
    note.id = doc.subject_id;
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note)
        .await
        .unwrap();
    let calls = model(&runtime, "ambiguous", false);
    assert!(
        matches!(runtime.repair_record_indexes(&token, doc.subject_id).await, Err(RuntimeError::InvalidInput(message)) if message.contains("ambiguous"))
    );
    assert!(runtime
        .text(&token)
        .unwrap()
        .get_document("local", doc.subject_id)
        .await
        .unwrap()
        .is_none());
    assert!(runtime
        .text_for_notes(&token)
        .unwrap()
        .get_document("local", doc.subject_id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(ann_count(&runtime).await, 0);
}

#[tokio::test]
async fn wrong_vector_kind_is_reported_and_preserved_without_embedding() {
    for entity in [false, true] {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = NamespaceToken::local();
        let record = seed(&runtime, &token, entity).await;
        let doc = record.document();
        let calls = model(&runtime, "occupied", false);
        let vectors = runtime.vectors_for_model(&token, "occupied").unwrap();
        vectors
            .insert(
                doc.subject_id,
                if entity {
                    SubstrateKind::Note
                } else {
                    SubstrateKind::Entity
                },
                "local",
                record.tables().2,
                vec![vec![0.9; 4]],
            )
            .await
            .unwrap();
        let before = ann_count(&runtime).await;
        let report = runtime
            .repair_record_indexes(&token, doc.subject_id)
            .await
            .unwrap();
        assert!(report.repaired.is_empty());
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].stage, "vector_presence");
        assert!(report.failures[0].error.contains("another vector identity"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(ann_count(&runtime).await, before);
        assert_eq!(
            vectors
                .get_vectors(&[doc.subject_id], "local", record.tables().2)
                .await
                .unwrap()[&doc.subject_id],
            vec![0.9; 4]
        );
    }
}

#[tokio::test]
async fn writer_rechecks_source_revision_and_deletion_before_every_repair() {
    for entity in [false, true] {
        for stage in ["fts", "vector"] {
            for deletion in [false, true] {
                let runtime = Arc::new(KhiveRuntime::memory().unwrap());
                let token = NamespaceToken::local();
                let record = seed(&runtime, &token, entity).await;
                let doc = record.document();
                if stage == "fts" {
                    text(&runtime, &token, entity)
                        .delete_document(&doc.namespace, doc.subject_id)
                        .await
                        .unwrap();
                } else {
                    model(&runtime, "race", false);
                }
                let barriers = Arc::new((Barrier::new(2), Barrier::new(2)));
                let repair_runtime = runtime.clone();
                let repair_token = token.clone();
                let id = doc.subject_id;
                let repair = tokio::spawn(BEFORE_PUBLICATION.scope(
                    (stage, barriers.clone()),
                    async move {
                        repair_runtime
                            .repair_record_indexes(&repair_token, id)
                            .await
                    },
                ));
                tokio::time::timeout(std::time::Duration::from_secs(10), barriers.0.wait())
                    .await
                    .expect("repair reached actual publication boundary");
                // Simulate the competing substrate writer without repairing its
                // indexes, to isolate the stale repair's version/liveness fence.
                runtime
                    .sql()
                    .writer()
                    .await
                    .unwrap()
                    .execute(
                        SqlStatement::new(
                            format!(
                                "UPDATE {} SET version=version+1{} WHERE id=?1",
                                record.tables().0,
                                if deletion { ", deleted_at=1" } else { "" }
                            ),
                            vec![SqlValue::Text(id.to_string())],
                        )
                        .labelled("record-index-repair"),
                    )
                    .await
                    .unwrap();
                barriers.1.wait().await;
                let report = repair.await.unwrap().unwrap();
                assert!(report.repaired.is_empty());
                assert_eq!(report.failures.len(), 1);
                assert_eq!(report.failures[0].stage, "source_revision");
                if stage == "fts" {
                    assert!(text(&runtime, &token, entity)
                        .get_document(&doc.namespace, id)
                        .await
                        .unwrap()
                        .is_none());
                } else {
                    assert!(runtime
                        .vectors_for_model(&token, "race")
                        .unwrap()
                        .get_vectors(&[id], &doc.namespace, record.tables().2)
                        .await
                        .unwrap()
                        .is_empty());
                }
            }
        }
    }
}

#[tokio::test]
async fn concurrent_healthy_publication_wins_without_replacement_or_extra_ann_write() {
    for entity in [false, true] {
        for stage in ["fts", "vector"] {
            let runtime = Arc::new(KhiveRuntime::memory().unwrap());
            let token = NamespaceToken::local();
            let record = seed(&runtime, &token, entity).await;
            let doc = record.document();
            if stage == "fts" {
                text(&runtime, &token, entity)
                    .delete_document(&doc.namespace, doc.subject_id)
                    .await
                    .unwrap();
            } else {
                model(&runtime, "race", false);
            }
            let barriers = Arc::new((Barrier::new(2), Barrier::new(2)));
            let repair_runtime = runtime.clone();
            let repair_token = token.clone();
            let id = doc.subject_id;
            let repair = tokio::spawn(BEFORE_PUBLICATION.scope(
                (stage, barriers.clone()),
                async move {
                    repair_runtime
                        .repair_record_indexes(&repair_token, id)
                        .await
                },
            ));
            tokio::time::timeout(std::time::Duration::from_secs(10), barriers.0.wait())
                .await
                .expect("repair reached actual publication boundary");
            if stage == "fts" {
                text(&runtime, &token, entity)
                    .upsert_document(doc.clone())
                    .await
                    .unwrap();
            } else {
                install_vector(&runtime, &token, &doc, "race", 0.95).await;
            }
            let before = ann_count(&runtime).await;
            barriers.1.wait().await;
            let report = repair.await.unwrap().unwrap();
            assert!(report.repaired.is_empty() && report.failures.is_empty());
            assert_eq!(ann_count(&runtime).await, before);
            if stage == "vector" {
                assert_eq!(
                    runtime
                        .vectors_for_model(&token, "race")
                        .unwrap()
                        .get_vectors(&[id], &doc.namespace, record.tables().2)
                        .await
                        .unwrap()[&id],
                    vec![0.95; 4]
                );
            }
        }
    }
}
