use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use khive_storage::note::Note;
use khive_types::Namespace;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::json;
use tokio::sync::Notify;

use crate::atomic_prepare::apply_post_commit_effects_with_report;
use crate::atomic_runner::{run_atomic_unit, AtomicOpPlan, AtomicRunOutcome};
use crate::curation::NotePatch;
use crate::embedder_registry::EmbedderProvider;
use crate::note_write::{NoteFence, NoteWriteOptions};
use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};

const MODEL: &str = "note-version-test";

#[test]
fn ordered_fences_cap_precedes_entry_validation_for_json_and_typed_inputs() {
    use crate::note_write::NoteFences;

    let entries: Vec<NoteFence> = (0..100)
        .map(|index| NoteFence {
            key: format!("fence-cap/{index}"),
            kind: "head".into(),
            expected_version: 1,
        })
        .collect();
    let at_cap = NoteFences::Many(entries.clone());
    at_cap.validate().unwrap();
    serde_json::from_value::<NoteFences>(serde_json::to_value(&at_cap).unwrap()).unwrap();

    let mut over_cap = entries;
    over_cap.push(NoteFence {
        key: "fence-cap/100".into(),
        kind: "head".into(),
        expected_version: 1,
    });
    for malformed in [false, true] {
        let mut entries = over_cap.clone();
        if malformed {
            entries[0].expected_version = 0;
        }
        let fences = NoteFences::Many(entries);
        let error = fences.validate().unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
        for message in [
            error.to_string(),
            serde_json::from_value::<NoteFences>(serde_json::to_value(&fences).unwrap())
                .unwrap_err()
                .to_string(),
        ] {
            assert!(message.contains("at most 100 entries"), "{message}");
            assert!(message.contains("sent 101"), "{message}");
        }
    }
    let malformed = json!(vec![serde_json::Value::Null; 101]);
    let message = serde_json::from_value::<NoteFences>(malformed)
        .unwrap_err()
        .to_string();
    assert!(message.contains("at most 100 entries"), "{message}");
    assert!(message.contains("sent 101"), "{message}");
}

#[tokio::test]
async fn ordered_fences_observe_prior_write_in_same_transaction() {
    let (runtime, token, _) = fixture();
    let lease_a = create(&runtime, &token, "lease/a", None).await;
    let lease_b = create(&runtime, &token, "lease/b", None).await;
    let target = create(&runtime, &token, "target", None).await;
    let lease_update = crate::atomic_prepare::prepare_update(
        &runtime,
        &token,
        &json!({"id":lease_b.id,"content":"{\"renewed\":true}","expected_version":1}),
        None,
    )
    .await
    .unwrap();
    let target_update = crate::atomic_prepare::prepare_update(
        &runtime,
        &token,
        &json!({"id":target.id,"content":"{\"bad\":true}","fence":[
            {"kind":"head","key":"lease/a","expected_version":1},
            {"kind":"head","key":"lease/b","expected_version":1}]}),
        None,
    )
    .await
    .unwrap();
    let outcome = run_atomic_unit(runtime.sql().as_ref(), vec![lease_update, target_update])
        .await
        .unwrap();
    let AtomicRunOutcome::RolledBack {
        failed_op_index,
        failure: crate::atomic_runner::AtomicOpFailure::NoteConflict(conflict),
        ..
    } = outcome
    else {
        panic!("fence must see the earlier update and roll back: {outcome:?}");
    };
    assert_eq!(failed_op_index, 1);
    let detail = serde_json::to_value(conflict.into_error().details().unwrap()).unwrap();
    assert_eq!(detail["index"], "1");
    assert_eq!(detail["current_version"], "2");
    for note in [lease_a, lease_b, target] {
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
}

#[derive(Default)]
struct Service {
    started: Notify,
    proceed: Notify,
    fail: AtomicBool,
}

#[async_trait]
impl EmbeddingService for Service {
    async fn embed(
        &self,
        texts: &[String],
        _: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.iter().any(|text| text.contains("inflight")) {
            self.started.notify_one();
            self.proceed.notified().await;
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(EmbedError::InferenceFailed("late test failure".into()));
        }
        Ok(texts.iter().map(|_| vec![0.5; 4]).collect())
    }
    fn supports_model(&self, _: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        MODEL
    }
}

struct Provider(Arc<Service>, String, usize);
#[async_trait]
impl EmbedderProvider for Provider {
    fn name(&self) -> &str {
        &self.1
    }
    fn dimensions(&self) -> usize {
        self.2
    }
    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(self.0.clone())
    }
}

fn fixture() -> (KhiveRuntime, NamespaceToken, Arc<Service>) {
    let runtime = KhiveRuntime::memory().unwrap();
    runtime.install_kind_registry(
        vec!["concept".into()],
        vec!["head".into(), "memory".into(), "observation".into()],
    );
    let token = runtime
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();
    let service = Arc::new(Service::default());
    runtime.register_embedder(Provider(service.clone(), MODEL.into(), 4));
    runtime.vectors_for_model(&token, MODEL).unwrap();
    (runtime, token, service)
}

async fn create(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    key: &str,
    embed: Option<bool>,
) -> Note {
    runtime
        .create_note_with_options(
            token,
            "head",
            None,
            "{}",
            None,
            None,
            None,
            None,
            vec![],
            None,
            NoteWriteOptions {
                key: Some(key.into()),
                embed,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .0
}

fn patch(content: &str, version: i64, embed: Option<bool>) -> NotePatch {
    NotePatch::new(None, Some(content.into()), None, None, None).with_write_options(
        NoteWriteOptions {
            expected_version: Some(version),
            embed,
            ..Default::default()
        },
    )
}

fn details(error: RuntimeError) -> serde_json::Value {
    let RuntimeError::Khive(error) = error else {
        panic!("expected structured error, got {error:?}");
    };
    serde_json::to_value(error.details().expect("conflict details")).unwrap()
}

async fn vectors(runtime: &KhiveRuntime, token: &NamespaceToken) -> u64 {
    runtime
        .vectors_for_model(token, MODEL)
        .unwrap()
        .count()
        .await
        .unwrap()
}

async fn ann_deletes(runtime: &KhiveRuntime, id: uuid::Uuid) -> i64 {
    let value = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_scalar(crate::note_write::statement(
            "SELECT COUNT(*) FROM ann_write_log WHERE subject_id=?1 AND op='delete'",
            vec![khive_storage::SqlValue::Text(id.to_string())],
        ))
        .await
        .unwrap()
        .unwrap();
    let khive_storage::SqlValue::Integer(count) = value else {
        panic!("invalid count")
    };
    count
}

#[tokio::test]
async fn version_guarded_vector_publication_canonicalizes_builtin_aliases() {
    let (runtime, token, service) = fixture();
    let model = EmbeddingModel::ParaphraseMultilingualMiniLmL12V2;
    runtime.register_embedder(Provider(service, model.to_string(), model.dimensions()));
    let note = create(&runtime, &token, "version/model-alias", None).await;
    let vector = vec![0.5; model.dimensions()];
    assert!(runtime
        .publish_note_vector_revision(&token, &note, "paraphrase", &vector)
        .await
        .unwrap());
    assert_eq!(
        runtime
            .vectors_for_model(&token, &model.to_string())
            .unwrap()
            .count()
            .await
            .unwrap(),
        1
    );
    let identity = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_scalar(crate::note_write::statement(
            "SELECT embedding_model FROM ann_write_log WHERE subject_id=?1 AND op='upsert'",
            vec![khive_storage::SqlValue::Text(note.id.to_string())],
        ))
        .await
        .unwrap();
    assert!(
        matches!(identity, Some(khive_storage::SqlValue::Text(name)) if name == model.to_string())
    );
}

#[tokio::test]
async fn version_guarded_vector_publication_checks_declared_dimensions() {
    let (runtime, token, service) = fixture();
    let note = create(&runtime, &token, "version/model-dimensions", None).await;
    // The old four-dimensional table remains while the provider is replaced.
    runtime.register_embedder(Provider(service, MODEL.into(), 8));
    let error = runtime
        .publish_note_vector_revision(&token, &note, MODEL, &[0.5; 4])
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("expected 8 vector dimensions"),
        "{error}"
    );
    assert_eq!(vectors(&runtime, &token).await, 0);
}

#[tokio::test]
async fn version_creation_compensation_preserves_or_removes_attachments_with_revision() {
    use khive_storage::attachment::{Attachment, AttachmentSubstrate};
    let (runtime, token, _) = fixture();
    for newer in [false, true] {
        let note = create(
            &runtime,
            &token,
            &format!("version/attachment-{newer}"),
            None,
        )
        .await;
        let attachment = Attachment {
            record_uuid: note.id,
            substrate: AttachmentSubstrate::Note,
            role: "source".into(),
            content_ref: khive_storage::ContentRef::from_hex("a".repeat(64)).unwrap(),
            media_type: None,
            size_bytes: None,
            created_at: 1,
        };
        runtime
            .sql()
            .writer()
            .await
            .unwrap()
            .execute(
                khive_db::stores::attachment::attachment_upsert_statement(&attachment).unwrap(),
            )
            .await
            .unwrap();
        if newer {
            runtime
                .update_note(&token, note.id, patch("{\"newer\":true}", 1, None))
                .await
                .unwrap();
        }
        assert_eq!(runtime.compensate_note_creation(&note).await, !newer);
        assert_eq!(
            runtime
                .notes(&token)
                .unwrap()
                .get_note(note.id)
                .await
                .unwrap()
                .is_some(),
            newer
        );
        let retained = runtime
            .sql()
            .reader()
            .await
            .unwrap()
            .query_scalar(crate::note_write::statement(
                "SELECT COUNT(*) FROM attachments WHERE record_uuid=?1",
                vec![khive_storage::SqlValue::Text(note.id.to_string())],
            ))
            .await
            .unwrap();
        assert!(
            matches!(retained, Some(khive_storage::SqlValue::Integer(count)) if count == i64::from(newer))
        );
    }
}

#[tokio::test]
async fn version_keyed_create_late_annotation_refusal_is_not_a_key_conflict() {
    use crate::atomic_message::{AtomicNoteOptions, AtomicNoteSpec};
    use crate::note_create::{prepare_note_create, KeyPublication};
    let (runtime, token, _) = fixture();
    let target = create(&runtime, &token, "version/annotation-target", None).await;
    let (prepared, _) = prepare_note_create(
        &runtime,
        AtomicNoteSpec {
            token: &token,
            id: None,
            kind: "head",
            name: None,
            content: "{}",
            properties: None,
        },
        AtomicNoteOptions {
            key: Some("version/annotation-child"),
            embed: Some(false),
            ..Default::default()
        },
        &[target.id],
        KeyPublication::AtInsert,
    )
    .await
    .unwrap();
    runtime
        .notes(&token)
        .unwrap()
        .delete_note(target.id, khive_storage::DeleteMode::Hard)
        .await
        .unwrap();
    let id = prepared.notes[0].id;
    let outcome = run_atomic_unit(runtime.sql().as_ref(), prepared.plans)
        .await
        .unwrap();
    assert!(
        matches!(
            outcome,
            AtomicRunOutcome::RolledBack {
                failure: crate::atomic_runner::AtomicOpFailure::GuardFailed { observed: 0, .. },
                ..
            }
        ),
        "annotation refusal must not disclose the rolled-back candidate as key holder: {outcome:?}"
    );
    assert!(runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn version_cas_rejects_already_stale_and_lost_ack_retry() {
    let (runtime, token, _) = fixture();
    let note = create(&runtime, &token, "version/cas", None).await;
    assert_eq!(note.version, 1);
    let updated = runtime
        .update_note(&token, note.id, patch("{\"winner\":true}", 1, None))
        .await
        .unwrap();
    assert_eq!(updated.version, 2);
    for body in ["{\"loser\":true}", "{\"winner\":true}"] {
        let error = runtime
            .update_note(&token, note.id, patch(body, 1, None))
            .await
            .unwrap_err();
        assert_eq!(
            details(error),
            json!({"reason":"version_conflict", "expected_version":"1", "current_version":"2"})
        );
        assert_eq!(
            runtime
                .notes(&token)
                .unwrap()
                .get_note(note.id)
                .await
                .unwrap()
                .unwrap(),
            updated
        );
    }
    assert_eq!(vectors(&runtime, &token).await, 0);
}

#[tokio::test]
async fn version_fence_and_prior_operation_roll_back_together() {
    let (runtime, token, _) = fixture();
    let fence = create(&runtime, &token, "version/fence", None).await;
    let target = create(&runtime, &token, "version/target", None).await;
    for (key, expected, current) in [
        ("version/fence", 2, Some("1")),
        ("version/missing", 1, None),
    ] {
        let mut update = patch("{\"changed\":true}", 1, None);
        update.write_options.fence = Some(
            NoteFence {
                key: key.into(),
                kind: "head".into(),
                expected_version: expected,
            }
            .into(),
        );
        let error = details(
            runtime
                .update_note(&token, target.id, update)
                .await
                .unwrap_err(),
        );
        assert_eq!(error["reason"], "fence_conflict");
        assert_eq!(
            error
                .get("current_version")
                .and_then(|value| value.as_str()),
            current
        );
        assert_eq!(
            runtime
                .notes(&token)
                .unwrap()
                .get_note(target.id)
                .await
                .unwrap()
                .unwrap(),
            target
        );
    }
    let (_, fence_plan) = runtime
        .prepare_versioned_note_update(&token, fence.clone(), patch("{\"renewed\":true}", 1, None))
        .await
        .unwrap();
    let mut update = patch("{\"changed\":true}", 1, None);
    update.write_options.fence = Some(
        NoteFence {
            key: fence.key.clone().unwrap(),
            kind: "head".into(),
            expected_version: 1,
        }
        .into(),
    );
    let (_, target_plan) = runtime
        .prepare_versioned_note_update(&token, target.clone(), update.clone())
        .await
        .unwrap();
    let outcome = run_atomic_unit(
        runtime.sql().as_ref(),
        vec![
            AtomicOpPlan::Update(fence_plan),
            AtomicOpPlan::Update(target_plan),
        ],
    )
    .await
    .unwrap();
    assert!(matches!(
        outcome,
        AtomicRunOutcome::RolledBack {
            failed_op_index: 1,
            ..
        }
    ));
    assert_eq!(
        runtime
            .notes(&token)
            .unwrap()
            .get_note(fence.id)
            .await
            .unwrap()
            .unwrap(),
        fence
    );
    assert_eq!(
        runtime
            .notes(&token)
            .unwrap()
            .get_note(target.id)
            .await
            .unwrap()
            .unwrap(),
        target
    );
    assert_eq!(
        runtime
            .update_note(&token, target.id, update)
            .await
            .unwrap()
            .version,
        2
    );
}

#[tokio::test]
async fn version_embedding_defaults_and_off_on_transitions() {
    async fn assert_surfaces(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        note: &Note,
        embedded: bool,
    ) {
        let candidates = runtime
            .vectors_for_model(token, MODEL)
            .unwrap()
            .search(khive_storage::VectorSearchRequest {
                query_vectors: vec![vec![0.5; 4]],
                top_k: 10,
                namespace: Some(token.namespace().as_str().to_string()),
                kind: Some(khive_types::SubstrateKind::Note),
                embedding_model: Some(MODEL.into()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert_eq!(
            candidates.iter().any(|hit| hit.subject_id == note.id),
            embedded,
            "similarity candidacy must follow the committed embedding state"
        );
        let lexical = runtime
            .text_for_notes(token)
            .unwrap()
            .search(khive_storage::TextSearchRequest {
                query: "phase".into(),
                mode: khive_storage::TextQueryMode::Plain,
                filter: None,
                top_k: 10,
                snippet_chars: 100,
            })
            .await
            .unwrap();
        assert!(
            lexical.iter().any(|hit| hit.subject_id == note.id),
            "embedding changes must retain lexical search results"
        );
        assert_eq!(
            runtime
                .list_notes(token, Some("head"), 10, 0)
                .await
                .unwrap(),
            vec![note.clone()],
            "embedding changes must retain the current note in listing"
        );
    }

    let (runtime, token, _) = fixture();
    let note = create(&runtime, &token, "version/embed", None).await;
    assert_eq!(vectors(&runtime, &token).await, 0);
    let note = runtime
        .update_note(&token, note.id, patch("{\"phase\":1}", 1, None))
        .await
        .unwrap();
    assert_eq!(vectors(&runtime, &token).await, 0);
    let note = runtime
        .update_note(
            &token,
            note.id,
            patch("{\"phase\":2}", note.version, Some(true)),
        )
        .await
        .unwrap();
    assert_eq!(vectors(&runtime, &token).await, 1);
    assert_surfaces(&runtime, &token, &note, true).await;
    let note = runtime
        .update_note(&token, note.id, patch("{\"phase\":3}", note.version, None))
        .await
        .unwrap();
    assert_eq!(vectors(&runtime, &token).await, 1);
    let note = runtime
        .update_note(
            &token,
            note.id,
            patch("{\"phase\":4}", note.version, Some(false)),
        )
        .await
        .unwrap();
    assert_eq!(vectors(&runtime, &token).await, 0);
    assert_surfaces(&runtime, &token, &note, false).await;
    assert_eq!(
        runtime
            .get_note_by_key(&token, "version/embed", Some("head"), false)
            .await
            .unwrap(),
        note
    );
    let note = runtime
        .update_note(
            &token,
            note.id,
            patch("{\"phase\":5}", note.version, Some(true)),
        )
        .await
        .unwrap();
    assert_eq!(vectors(&runtime, &token).await, 1);
    assert_surfaces(&runtime, &token, &note, true).await;
}

#[tokio::test]
async fn version_delayed_reindex_cannot_reverse_embed_off() {
    let (runtime, token, _) = fixture();
    let note = create(&runtime, &token, "version/delayed", Some(true)).await;
    let (_, plan) = runtime
        .prepare_versioned_note_update(&token, note.clone(), patch("{\"old\":true}", 1, Some(true)))
        .await
        .unwrap();
    let AtomicRunOutcome::Committed { post_commit } =
        run_atomic_unit(runtime.sql().as_ref(), vec![AtomicOpPlan::Update(plan)])
            .await
            .unwrap()
    else {
        panic!("commit");
    };
    runtime
        .update_note(&token, note.id, patch("{\"off\":true}", 2, Some(false)))
        .await
        .unwrap();
    assert!(
        apply_post_commit_effects_with_report(&runtime, &token, post_commit)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(vectors(&runtime, &token).await, 0);
    assert_eq!(
        runtime
            .notes(&token)
            .unwrap()
            .get_note(note.id)
            .await
            .unwrap()
            .unwrap()
            .content,
        "{\"off\":true}"
    );
}

#[tokio::test]
async fn version_embedding_inheritance_includes_retired_model_rows() {
    assert_writer_time_embedding_inheritance(true).await;
}

#[tokio::test]
async fn version_embedding_inheritance_sees_publication_after_prepare() {
    assert_writer_time_embedding_inheritance(false).await;
}

async fn assert_writer_time_embedding_inheritance(retired: bool) {
    let (runtime, token, _) = fixture();
    let note = create(&runtime, &token, "version/inherited", None).await;
    let (_, plan) = runtime
        .prepare_versioned_note_update(&token, note.clone(), patch("{\"new\":true}", 1, None))
        .await
        .unwrap();
    if retired {
        runtime
            .backend()
            .vectors_for_namespace("retired_chunks", "retired", 4, "local")
            .unwrap()
            .insert(
                note.id,
                khive_types::SubstrateKind::Note,
                "local",
                "note.content",
                vec![vec![0.1; 4]],
            )
            .await
            .unwrap();
    } else {
        assert!(runtime
            .publish_note_vector_revision(&token, &note, MODEL, &[0.1; 4])
            .await
            .unwrap());
    }
    // Index-only publication does not change the note's CAS precondition.
    assert_eq!(
        runtime
            .notes(&token)
            .unwrap()
            .get_note(note.id)
            .await
            .unwrap()
            .unwrap()
            .version,
        1
    );
    let AtomicRunOutcome::Committed { post_commit } =
        run_atomic_unit(runtime.sql().as_ref(), vec![AtomicOpPlan::Update(plan)])
            .await
            .unwrap()
    else {
        panic!("commit")
    };
    assert_eq!(
        post_commit.as_slice(),
        &[crate::PostCommitEffect::ReindexNote {
            note_id: note.id,
            version: 2
        }]
    );
    apply_post_commit_effects_with_report(&runtime, &token, post_commit)
        .await
        .unwrap();
    let stored = runtime.sql().reader().await.unwrap().query_scalar(crate::note_write::statement(
        "SELECT vec_to_json(embedding) FROM vec_note_version_test WHERE namespace=?1 AND subject_id=?2",
        vec![khive_storage::SqlValue::Text("local".into()), khive_storage::SqlValue::Text(note.id.to_string())],
    )).await.unwrap().unwrap();
    let khive_storage::SqlValue::Text(stored) = stored else {
        panic!("vector JSON")
    };
    assert_eq!(
        serde_json::from_str::<Vec<f32>>(&stored).unwrap(),
        vec![0.5; 4]
    );
}

#[tokio::test]
async fn version_embedding_inheritance_rollback_has_no_effect_token() {
    let (runtime, token, _) = fixture();
    let note = create(&runtime, &token, "version/inherit-rollback", None).await;
    let (_, plan) = runtime
        .prepare_versioned_note_update(&token, note.clone(), patch("{\"new\":true}", 1, None))
        .await
        .unwrap();
    assert!(runtime
        .publish_note_vector_revision(&token, &note, MODEL, &[0.1; 4])
        .await
        .unwrap());
    let outcome = run_atomic_unit(
        runtime.sql().as_ref(),
        vec![
            AtomicOpPlan::Update(plan.clone()),
            AtomicOpPlan::Update(plan),
        ],
    )
    .await
    .unwrap();
    assert!(matches!(
        outcome,
        AtomicRunOutcome::RolledBack {
            failed_op_index: 1,
            ..
        }
    ));
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
    assert_eq!(vectors(&runtime, &token).await, 1);
}

#[tokio::test]
async fn version_inflight_reindex_rechecks_inside_its_writer_transaction() {
    let (runtime, token, service) = fixture();
    let note = create(&runtime, &token, "version/inflight", Some(true)).await;
    let rt = runtime.clone();
    let tok = token.clone();
    let id = note.id;
    let update = tokio::spawn(async move {
        rt.update_note(&tok, id, patch("{\"inflight\":true}", 1, Some(true)))
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), service.started.notified())
        .await
        .unwrap();
    let off = runtime
        .update_note(&token, id, patch("{\"off\":true}", 2, Some(false)))
        .await
        .unwrap();
    service.proceed.notify_one();
    tokio::time::timeout(Duration::from_secs(10), update)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(vectors(&runtime, &token).await, 0);
    assert_eq!(
        runtime
            .notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap(),
        off
    );
}

#[tokio::test]
async fn version_embedding_purge_rolls_back_with_a_later_conflict() {
    let (runtime, token, _) = fixture();
    let note = create(&runtime, &token, "version/rollback", Some(true)).await;
    let retired = runtime
        .backend()
        .vectors_for_namespace(
            "old_model_chunks",
            "old-model-chunks",
            4,
            token.namespace().as_str(),
        )
        .unwrap();
    retired
        .insert(
            note.id,
            khive_types::SubstrateKind::Note,
            token.namespace().as_str(),
            "note.content",
            vec![vec![0.5; 4]],
        )
        .await
        .unwrap();
    assert_eq!(ann_deletes(&runtime, note.id).await, 0);
    let (_, off) = runtime
        .prepare_versioned_note_update(
            &token,
            note.clone(),
            patch("{\"off\":true}", 1, Some(false)),
        )
        .await
        .unwrap();
    let (_, stale) = runtime
        .prepare_versioned_note_update(&token, note.clone(), patch("{\"late\":true}", 1, None))
        .await
        .unwrap();
    assert!(matches!(
        run_atomic_unit(
            runtime.sql().as_ref(),
            vec![AtomicOpPlan::Update(off), AtomicOpPlan::Update(stale)]
        )
        .await
        .unwrap(),
        AtomicRunOutcome::RolledBack {
            failed_op_index: 1,
            ..
        }
    ));
    assert_eq!(vectors(&runtime, &token).await, 1);
    assert_eq!(retired.count().await.unwrap(), 1);
    assert_eq!(
        ann_deletes(&runtime, note.id).await,
        0,
        "delete log must roll back with vectors"
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
async fn version_guard_observes_a_write_after_prepare_without_timestamp_change() {
    let (runtime, token, _) = fixture();
    let note = create(&runtime, &token, "version/prepare-race", None).await;
    let (_, plan) = runtime
        .prepare_versioned_note_update(&token, note.clone(), patch("{\"loser\":true}", 1, None))
        .await
        .unwrap();
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(crate::note_write::statement(
            "UPDATE notes SET properties='{}' WHERE id=?1",
            vec![khive_storage::SqlValue::Text(note.id.to_string())],
        ))
        .await
        .unwrap();
    let current = runtime
        .notes(&token)
        .unwrap()
        .get_note(note.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.updated_at, note.updated_at);
    assert_eq!(current.version, 2);
    let outcome = run_atomic_unit(runtime.sql().as_ref(), vec![AtomicOpPlan::Update(plan)])
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        AtomicRunOutcome::RolledBack {
            failure: crate::atomic_runner::AtomicOpFailure::NoteConflict(
                crate::note_write::NoteWriteConflict::Version {
                    expected: 1,
                    current: 2
                }
            ),
            ..
        }
    ));
    assert_eq!(
        runtime
            .notes(&token)
            .unwrap()
            .get_note(note.id)
            .await
            .unwrap()
            .unwrap(),
        current
    );
}

#[tokio::test]
async fn version_off_removes_unregistered_vectors_created_after_prepare() {
    let (runtime, token, _) = fixture();
    let note = create(&runtime, &token, "version/retired-model", Some(true)).await;
    let control = create(&runtime, &token, "version/retired-control", None).await;
    let (_, plan) = runtime
        .prepare_versioned_note_update(
            &token,
            note.clone(),
            patch("{\"off\":true}", 1, Some(false)),
        )
        .await
        .unwrap();
    let retired = runtime
        .backend()
        .vectors_for_namespace(
            "retired_note_model_chunks",
            "retired-note-model",
            4,
            token.namespace().as_str(),
        )
        .unwrap();
    for id in [note.id, control.id] {
        retired
            .insert(
                id,
                khive_types::SubstrateKind::Note,
                token.namespace().as_str(),
                "note.content",
                vec![vec![0.5; 4]],
            )
            .await
            .unwrap();
    }
    assert_eq!(retired.count().await.unwrap(), 2);
    let foreign = runtime
        .backend()
        .vectors_for_namespace(
            "retired_note_model_chunks",
            "retired-note-model",
            4,
            "foreign",
        )
        .unwrap();
    let foreign_id = uuid::Uuid::new_v4();
    foreign
        .insert(
            foreign_id,
            khive_types::SubstrateKind::Note,
            "foreign",
            "note.content",
            vec![vec![0.5; 4]],
        )
        .await
        .unwrap();
    assert!(!runtime
        .registered_embedding_model_names()
        .contains(&"retired-note-model".into()));
    let outcome = run_atomic_unit(runtime.sql().as_ref(), vec![AtomicOpPlan::Update(plan)])
        .await
        .unwrap();
    assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));
    assert_eq!(vectors(&runtime, &token).await, 0);
    assert_eq!(
        retired.count().await.unwrap(),
        1,
        "explicit off must remove the target from persisted unregistered model tables"
    );
    let retained = retired
        .batch_exists(&[control.id, note.id], token.namespace().as_str())
        .await
        .unwrap();
    assert!(retained.contains(&control.id));
    assert!(!retained.contains(&note.id));
    assert_eq!(foreign.count().await.unwrap(), 1);
    assert_eq!(ann_deletes(&runtime, foreign_id).await, 0);
    assert_eq!(ann_deletes(&runtime, control.id).await, 0);
    assert_eq!(
        ann_deletes(&runtime, note.id).await,
        2,
        "one delete delta per purged model row"
    );
}

#[tokio::test]
async fn version_legacy_create_cannot_publish_vectors_after_explicit_off() {
    assert_legacy_creation_revision_guard(false, false).await;
}

#[tokio::test]
async fn version_legacy_multimodel_create_cannot_publish_after_explicit_off() {
    assert_legacy_creation_revision_guard(true, false).await;
}

#[tokio::test]
async fn version_legacy_embedding_failure_preserves_newer_note() {
    assert_legacy_creation_revision_guard(false, true).await;
}

#[tokio::test]
async fn version_legacy_multimodel_embedding_failure_preserves_newer_note() {
    assert_legacy_creation_revision_guard(true, true).await;
}

async fn assert_legacy_creation_revision_guard(multimodel: bool, fail: bool) {
    let (runtime, token, service) = fixture();
    let second = Arc::new(Service::default());
    if multimodel {
        runtime.register_embedder(Provider(second.clone(), "second-note-model".into(), 4));
        runtime
            .vectors_for_model(&token, "second-note-model")
            .unwrap();
    }
    let rt = runtime.clone();
    let tok = token.clone();
    let create = tokio::spawn(async move {
        rt.create_note(
            &tok,
            "observation",
            None,
            "inflight legacy creation",
            None,
            None,
            vec![],
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(10), service.started.notified())
        .await
        .unwrap();
    if multimodel {
        tokio::time::timeout(Duration::from_secs(10), second.started.notified())
            .await
            .unwrap();
    }
    let notes = runtime
        .list_notes(&token, Some("observation"), 2, 0)
        .await
        .unwrap();
    assert_eq!(notes.len(), 1);
    let id = notes[0].id;
    let off = runtime
        .update_note(
            &token,
            id,
            patch("off while creation embeds", 1, Some(false)),
        )
        .await
        .unwrap();
    service.fail.store(fail, Ordering::SeqCst);
    service.proceed.notify_one();
    second.proceed.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(10), create)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.is_err(), fail, "creation outcome: {result:?}");
    assert_eq!(
        runtime
            .notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap(),
        off
    );
    assert_eq!(
        vectors(&runtime, &token).await,
        0,
        "creation's stale embedding must not reverse the later off-update"
    );
    if multimodel {
        assert_eq!(
            runtime
                .vectors_for_model(&token, "second-note-model")
                .unwrap()
                .count()
                .await
                .unwrap(),
            0
        );
    }
    let hits = runtime
        .text_for_notes(&token)
        .unwrap()
        .search(khive_storage::TextSearchRequest {
            query: "off while creation".into(),
            mode: khive_storage::TextQueryMode::Plain,
            filter: None,
            top_k: 10,
            snippet_chars: 100,
        })
        .await
        .unwrap();
    assert!(
        hits.iter().any(|hit| hit.subject_id == id),
        "newer lexical document must survive"
    );
}

#[path = "note_fence_race_tests.rs"]
mod fence_races;
