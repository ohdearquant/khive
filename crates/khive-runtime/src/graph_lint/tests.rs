use super::*;
use crate::{
    KhiveRuntime, Namespace, NamespaceToken, RuntimeConfig, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{Edge, EdgeRelation, Entity, Note, SqlAccess, SqlStatement, SqlValue};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use uuid::Uuid;

fn runtime() -> KhiveRuntime {
    KhiveRuntime::new_for_test(RuntimeConfig {
        db_path: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap()
}
fn registry() -> VerbRegistry {
    VerbRegistryBuilder::new().build().unwrap()
}
fn token(runtime: &KhiveRuntime) -> NamespaceToken {
    runtime.authorize(Namespace::local()).unwrap()
}
async fn entity(runtime: &KhiveRuntime, name: &str) -> Entity {
    let record =
        Entity::new("local", "concept", name).with_properties(json!({"keep":{"nested":[1,2]}}));
    runtime
        .entities(&token(runtime))
        .unwrap()
        .upsert_entity(record.clone())
        .await
        .unwrap();
    record
}
async fn edge(
    runtime: &KhiveRuntime,
    source_id: Uuid,
    target_id: Uuid,
    relation: EdgeRelation,
) -> Edge {
    let now = chrono::Utc::now();
    let record = Edge {
        id: Uuid::new_v4().into(),
        namespace: "local".into(),
        source_id,
        target_id,
        relation,
        weight: 1.0,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: Some(json!({"keep":"edge"})),
        target_backend: None,
    };
    runtime
        .graph(&token(runtime))
        .unwrap()
        .upsert_edge(record.clone())
        .await
        .unwrap();
    runtime
        .get_edge(&token(runtime), record.id.into())
        .await
        .unwrap()
        .unwrap()
}
async fn execute(runtime: &KhiveRuntime, sql: &str, params: Vec<SqlValue>) {
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement::new(sql, params))
        .await
        .unwrap();
}
fn violation(
    subject: LintSubject,
    rule: LintRuleId,
    member_ids: Option<Vec<Uuid>>,
) -> LintViolation {
    LintViolation {
        subject,
        rule,
        severity: LintSeverity::Error,
        message: "fixture observation".into(),
        suggestion: "fixture action".into(),
        auto_fixable: true,
        member_ids,
    }
}
fn entity_subject(entity: &Entity) -> LintSubject {
    LintSubject::Entity {
        namespace: entity.namespace.clone(),
        entity_id: entity.id,
        entity_name: entity.name.clone(),
        entity_kind: entity.kind.clone(),
    }
}
fn report(violations: Vec<LintViolation>, capture: &LintCapture) -> LintReport {
    LintReport {
        summary: LintSummary {
            total: violations.len() as u64,
            errors: violations.len() as u64,
            ..Default::default()
        },
        violations,
        stats: LintStats {
            total_entities: capture.entities().len() as u64,
            total_notes: capture.notes().len() as u64,
            total_edges: capture.edges().len() as u64,
            ..Default::default()
        },
    }
}
struct Fixture {
    runtime: KhiveRuntime,
    token: NamespaceToken,
    registry: VerbRegistry,
    dead: Entity,
    duplicate: Vec<Entity>,
    old: Entity,
    dangling: Edge,
    supersedes: Edge,
}
impl Fixture {
    async fn new(runtime: KhiveRuntime) -> Self {
        let token = token(&runtime);
        let registry = registry();
        let dead = entity(&runtime, "deleted endpoint").await;
        let survivor = entity(&runtime, "live endpoint").await;
        let old = entity(&runtime, "superseded").await;
        let newer = entity(&runtime, "replacement").await;
        let mut duplicate = vec![
            entity(&runtime, "same exact name").await,
            entity(&runtime, "same exact name").await,
        ];
        duplicate.sort_by_key(|entity| entity.id);
        let dangling = edge(&runtime, dead.id, survivor.id, EdgeRelation::Extends).await;
        let supersedes = edge(&runtime, newer.id, old.id, EdgeRelation::Supersedes).await;
        execute(
            &runtime,
            "UPDATE entities SET deleted_at=17, version=version+1 WHERE id=?1",
            vec![SqlValue::Text(dead.id.to_string())],
        )
        .await;
        Self {
            runtime,
            token,
            registry,
            dead,
            duplicate,
            old,
            dangling,
            supersedes,
        }
    }
    async fn capture_report(&self) -> (LintCapture, LintReport) {
        let capture = self
            .registry
            .capture_graph_lint(&self.runtime, &self.token)
            .await
            .unwrap();
        let report = report(
            vec![
                violation(
                    LintSubject::Edge {
                        namespace: "local".into(),
                        edge_id: self.dangling.id.into(),
                    },
                    LintRuleId::DanglingEdge,
                    None,
                ),
                violation(
                    entity_subject(&self.duplicate[0]),
                    LintRuleId::DuplicateName,
                    Some(self.duplicate.iter().map(|e| e.id).collect()),
                ),
                violation(
                    entity_subject(&self.old),
                    LintRuleId::SupersededNotMarked,
                    None,
                ),
            ],
            &capture,
        );
        (capture, report)
    }
}

#[tokio::test]
async fn all_three_fixes_keep_pre_fix_report_and_normal_events_on_direct_and_queued_owners() {
    let directory = tempfile::tempdir().unwrap();
    let queued = KhiveRuntime::new_with_file_backend(
        RuntimeConfig {
            db_path: Some(directory.path().join("queued.db")),
            ..RuntimeConfig::no_embeddings()
        },
        true,
        |path| {
            khive_db::StorageBackend::sqlite_for_test_with_journal_mode_in(
                path,
                true,
                std::time::Duration::from_secs(1),
                directory.path().join("leases"),
            )
        },
    )
    .unwrap();
    for runtime in [runtime(), queued] {
        let fixture = Fixture::new(runtime).await;
        let (capture, report) = fixture.capture_report().await;
        let before = report.clone();
        let (after, receipts) = capture
            .apply_fixes(&fixture.registry, &fixture.token, report)
            .await
            .unwrap();
        assert_eq!(after.stats, before.stats);
        assert_eq!(after.violations, before.violations);
        assert_eq!(after.summary.fixed, Some(3));
        assert_eq!(receipts.len(), 3);
        assert!(receipts.iter().all(FixReceipt::is_committed));
        assert!(fixture
            .runtime
            .get_edge(&fixture.token, fixture.dangling.id.into())
            .await
            .unwrap()
            .is_none());
        let updated = fixture
            .runtime
            .get_entity(&fixture.token, fixture.old.id)
            .await
            .unwrap();
        assert_eq!(
            updated.properties,
            Some(json!({"keep":{"nested":[1,2]},"status":"deprecated"}))
        );
        let FixReceipt::Committed {
            action:
                CommittedFixAction::ProposeDuplicateMerge {
                    question_note_id, ..
                },
        } = receipts[1]
        else {
            panic!("question receipt missing id")
        };
        let note = fixture
            .runtime
            .notes(&fixture.token)
            .unwrap()
            .get_note(question_note_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(note.kind, "question");
        assert_eq!(note.namespace, "local");
        assert_eq!(
            note.properties.as_ref().unwrap()["member_ids"],
            json!(fixture
                .duplicate
                .iter()
                .map(|entity| entity.id)
                .collect::<Vec<_>>())
        );
        for member in &fixture.duplicate {
            assert!(fixture
                .runtime
                .get_entity(&fixture.token, member.id)
                .await
                .is_ok());
        }
        let events = fixture
            .runtime
            .list_events(
                &fixture.token,
                khive_storage::EventFilter::default(),
                khive_storage::PageRequest {
                    offset: 0,
                    limit: 100,
                },
            )
            .await
            .unwrap()
            .items;
        assert!(events
            .iter()
            .any(|event| event.kind == khive_types::EventKind::NoteCreated
                && event.target_id == Some(question_note_id)
                && event.payload["kind"] == "question"));
        assert!(events
            .iter()
            .any(|event| event.kind == khive_types::EventKind::EdgeDeleted
                && event.target_id == Some(fixture.dangling.id.into())));
        assert!(events
            .iter()
            .any(|event| event.kind == khive_types::EventKind::EntityUpdated
                && event.target_id == Some(fixture.old.id)));
        let wire = serde_json::to_value(&receipts).unwrap();
        assert!(wire[0].get("question_note_id").is_none());
        assert!(wire[2].get("question_note_id").is_none());
    }
}

#[tokio::test]
async fn changed_tombstone_group_revision_and_supersedes_witness_conflict_without_writes() {
    let fixture = Fixture::new(runtime()).await;
    let (capture, report) = fixture.capture_report().await;
    execute(
        &fixture.runtime,
        "UPDATE entities SET deleted_at=NULL,version=version+1 WHERE id=?1",
        vec![SqlValue::Text(fixture.dead.id.to_string())],
    )
    .await;
    execute(
        &fixture.runtime,
        "UPDATE entities SET name='renamed',version=version+1 WHERE id=?1",
        vec![SqlValue::Text(fixture.duplicate[0].id.to_string())],
    )
    .await;
    execute(
        &fixture.runtime,
        "UPDATE graph_edges SET weight=0.5 WHERE id=?1",
        vec![SqlValue::Text(
            Uuid::from(fixture.supersedes.id).to_string(),
        )],
    )
    .await;
    let (report, receipts) = capture
        .apply_fixes(&fixture.registry, &fixture.token, report)
        .await
        .unwrap();
    assert_eq!(report.summary.fixed, Some(0));
    assert!(receipts
        .iter()
        .all(|receipt| matches!(receipt, FixReceipt::Conflict { .. })));
    assert!(fixture
        .runtime
        .get_edge(&fixture.token, fixture.dangling.id.into())
        .await
        .unwrap()
        .is_some());
    assert!(fixture
        .runtime
        .list_notes(&fixture.token, Some("question"), 10, 0)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        fixture
            .runtime
            .get_entity(&fixture.token, fixture.old.id)
            .await
            .unwrap()
            .properties,
        fixture.old.properties
    );
}

#[tokio::test]
async fn duplicate_arrivals_do_not_invalidate_the_observed_group() {
    let fixture = Fixture::new(runtime()).await;
    let (capture, mut report) = fixture.capture_report().await;
    report.violations = vec![report.violations[1].clone()];
    let late = entity(&fixture.runtime, "same exact name").await;
    let (_, receipts) = capture
        .apply_fixes(&fixture.registry, &fixture.token, report)
        .await
        .unwrap();
    assert!(receipts[0].is_committed());
    let FixReceipt::Committed {
        action:
            CommittedFixAction::ProposeDuplicateMerge {
                question_note_id, ..
            },
    } = receipts[0]
    else {
        panic!()
    };
    let note = fixture
        .runtime
        .notes(&fixture.token)
        .unwrap()
        .get_note(question_note_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!note.content.contains(&late.id.to_string()));
}

#[tokio::test]
async fn capture_paginates_all_rows_and_hides_mailbox_and_namespace_edges() {
    let runtime = runtime();
    let token = runtime
        .authorize_with_visibility(
            Namespace::local(),
            vec![Namespace::parse("visible").unwrap()],
        )
        .unwrap();
    for index in 0..260 {
        entity(&runtime, &format!("page-{index}")).await;
    }
    let mut visible_note = Note::new("visible", "observation", "visible note");
    let hidden_note = Note::new("local", "message", "private mail")
        .with_properties(json!({"direction":"inbound","to_actor":"other"}));
    runtime
        .raw_notes(&token)
        .unwrap()
        .upsert_note(visible_note.clone())
        .await
        .unwrap();
    runtime
        .raw_notes(&token)
        .unwrap()
        .upsert_note(hidden_note.clone())
        .await
        .unwrap();
    let target = entity(&runtime, "target").await;
    let hidden_edge = edge(&runtime, hidden_note.id, target.id, EdgeRelation::Annotates).await;
    let visible_edge = edge(
        &runtime,
        visible_note.id,
        target.id,
        EdgeRelation::Annotates,
    )
    .await;
    let secret = Entity::new("outside", "concept", "hidden entity");
    runtime
        .entities(&token)
        .unwrap()
        .upsert_entity(secret.clone())
        .await
        .unwrap();
    let hidden_namespace_edge = edge(&runtime, target.id, secret.id, EdgeRelation::Extends).await;
    let capture = registry()
        .capture_graph_lint(&runtime, &token)
        .await
        .unwrap();
    assert_eq!(capture.entities().len(), 261);
    assert_eq!(capture.notes().len(), 1);
    assert_eq!(capture.edges().len(), 1);
    assert_eq!(capture.edges()[0].edge.id, visible_edge.id);
    assert!(capture
        .edges()
        .iter()
        .all(|edge| edge.edge.id != hidden_edge.id && edge.edge.id != hidden_namespace_edge.id));
    visible_note.deleted_at = Some(23);
    runtime
        .raw_notes(&token)
        .unwrap()
        .upsert_note(visible_note)
        .await
        .unwrap();
    let capture = registry()
        .capture_graph_lint(&runtime, &token)
        .await
        .unwrap();
    assert!(capture.notes().is_empty());
    assert!(capture.edges()[0].source_soft_deleted);
    assert!(!capture.edges()[0].source_live);
    assert_eq!(capture.edges()[0].contract_valid, Some(true));
}

#[tokio::test]
async fn absent_endpoint_is_not_a_tombstone_and_edge_event_annotation_liveness_is_resolved() {
    let runtime = runtime();
    let token = token(&runtime);
    let source = entity(&runtime, "source").await;
    let absent = edge(&runtime, source.id, Uuid::new_v4(), EdgeRelation::Extends).await;
    let note = Note::new("local", "observation", "annotations");
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    let event = khive_storage::Event::new(
        "local",
        "create",
        khive_types::EventKind::NoteCreated,
        khive_types::SubstrateKind::Note,
        "",
    );
    runtime
        .events(&token)
        .unwrap()
        .append_event(event.clone())
        .await
        .unwrap();
    let to_event = edge(&runtime, note.id, event.id, EdgeRelation::Annotates).await;
    let to_edge = edge(&runtime, note.id, absent.id.into(), EdgeRelation::Annotates).await;
    let capture = registry()
        .capture_graph_lint(&runtime, &token)
        .await
        .unwrap();
    let absent = capture
        .edges()
        .iter()
        .find(|edge| edge.edge.id == absent.id)
        .unwrap();
    assert!(!absent.target_live);
    assert!(!absent.target_soft_deleted);
    assert_eq!(absent.contract_valid, None);
    for id in [to_event.id, to_edge.id] {
        let edge = capture
            .edges()
            .iter()
            .find(|edge| edge.edge.id == id)
            .unwrap();
        assert!(edge.source_live && edge.target_live);
        assert_eq!(edge.contract_valid, Some(true));
    }
}

#[tokio::test]
async fn physical_aliases_deduplicate_but_equal_rows_on_distinct_owners_cannot_be_mutated() {
    let first = runtime();
    let second = runtime();
    let token = token(&first);
    let a = entity(&first, "duplicate").await;
    let b = entity(&first, "duplicate").await;
    for record in [&a, &b] {
        second
            .entities(&token)
            .unwrap()
            .upsert_entity(record.clone())
            .await
            .unwrap();
    }
    let registry = crate::pack::registry_with_lint_owners(
        &first,
        std::collections::HashMap::from([
            ("alias".into(), first.clone()),
            ("second".into(), second.clone()),
        ]),
    );
    let capture = registry.capture_graph_lint(&first, &token).await.unwrap();
    assert_eq!(capture.entities().len(), 2);
    assert_eq!(capture.owners.len(), 2);
    let mut records = [a, b];
    records.sort_by_key(|record| record.id);
    let report = report(
        vec![violation(
            entity_subject(&records[0]),
            LintRuleId::DuplicateName,
            Some(records.iter().map(|record| record.id).collect()),
        )],
        &capture,
    );
    let (_, receipts) = capture
        .apply_fixes(&registry, &token, report)
        .await
        .unwrap();
    assert!(matches!(receipts[0], FixReceipt::Unsupported { .. }));
    let mut contradictory = records[0].clone();
    contradictory.name = "contradiction".into();
    second
        .entities(&token)
        .unwrap()
        .upsert_entity(contradictory)
        .await
        .unwrap();
    assert!(matches!(
        registry.capture_graph_lint(&first, &token).await,
        Err(crate::RuntimeError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn equal_uuid_in_different_substrates_is_an_inspection_error() {
    let runtime = runtime();
    let token = token(&runtime);
    let record = entity(&runtime, "entity").await;
    let mut note = Note::new("local", "observation", "same UUID");
    note.id = record.id;
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note)
        .await
        .unwrap();
    assert!(matches!(
        registry().capture_graph_lint(&runtime, &token).await,
        Err(crate::RuntimeError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn listed_endpoint_sharing_an_event_uuid_is_an_inspection_error() {
    let runtime = runtime();
    let token = token(&runtime);
    let event = khive_storage::Event::new(
        "local",
        "create",
        khive_types::EventKind::NoteCreated,
        khive_types::SubstrateKind::Note,
        "",
    );
    runtime
        .events(&token)
        .unwrap()
        .append_event(event.clone())
        .await
        .unwrap();
    let mut note = Note::new("local", "observation", "same UUID as an event");
    note.id = event.id;
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    let target = entity(&runtime, "annotated").await;
    edge(&runtime, note.id, target.id, EdgeRelation::Annotates).await;
    assert!(matches!(
        registry().capture_graph_lint(&runtime, &token).await,
        Err(crate::RuntimeError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn expired_request_never_returns_a_partial_success() {
    let runtime = runtime();
    let token = token(&runtime);
    let result = khive_storage::scope_request_read_deadline(
        std::time::Duration::ZERO,
        registry().capture_graph_lint(&runtime, &token),
    )
    .await;
    assert!(result.is_err());
}

struct FaultAccess {
    inner: Arc<dyn SqlAccess>,
    calls: Arc<AtomicUsize>,
    unknown_at: Option<usize>,
    before_first: Option<SqlStatement>,
    fail_reindex: bool,
}
#[async_trait::async_trait]
impl SqlAccess for FaultAccess {
    async fn reader(&self) -> khive_storage::StorageResult<Box<dyn khive_storage::SqlReader>> {
        self.inner.reader().await
    }
    async fn writer(&self) -> khive_storage::StorageResult<Box<dyn khive_storage::SqlWriter>> {
        self.inner.writer().await
    }
    async fn atomic_unit(
        &self,
        operation: khive_storage::AtomicUnitOp,
    ) -> khive_storage::StorageResult<Box<dyn std::any::Any + Send>> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        if index == 0 {
            if let Some(statement) = &self.before_first {
                self.inner
                    .writer()
                    .await?
                    .execute(statement.clone())
                    .await?;
            }
        }
        let result = self.inner.atomic_unit(operation).await;
        if self.fail_reindex && result.is_ok() {
            self.inner
                .writer()
                .await?
                .execute(SqlStatement::new("DROP TABLE fts_notes", vec![]))
                .await?;
        }
        if self.unknown_at == Some(index) {
            assert!(
                result.is_ok(),
                "fixture must lose a real commit acknowledgement"
            );
            return Err(khive_storage::StorageError::WriterTaskRequestFailed {
                request_state: khive_storage::WriterTaskRequestState::SideEffectsUnknown,
                source: Box::new(khive_storage::StorageError::Internal(
                    "lost acknowledgement".into(),
                )),
            });
        }
        result
    }
}

#[tokio::test]
async fn recheck_inside_actual_writer_refuses_race_after_all_preparation() {
    let fixture = Fixture::new(runtime()).await;
    let (capture, mut report) = fixture.capture_report().await;
    report.violations.truncate(1);
    let calls = Arc::new(AtomicUsize::new(0));
    let access: Arc<dyn SqlAccess> = Arc::new(FaultAccess {
        inner: fixture.runtime.sql(),
        calls: calls.clone(),
        unknown_at: None,
        fail_reindex: false,
        before_first: Some(SqlStatement::new(
            "UPDATE entities SET deleted_at=NULL,version=version+1 WHERE id=?1",
            vec![SqlValue::Text(fixture.dead.id.to_string())],
        )),
    });
    let (_, receipts) = super::fix::TEST_ACTION_ACCESS
        .scope(
            access,
            capture.apply_fixes(&fixture.registry, &fixture.token, report),
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(receipts[0], FixReceipt::Conflict { .. }));
    assert!(fixture
        .runtime
        .get_edge(&fixture.token, fixture.dangling.id.into())
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn unknown_commit_stops_without_replay_and_retains_prior_receipt_and_report() {
    let fixture = Fixture::new(runtime()).await;
    let (capture, report) = fixture.capture_report().await;
    let before = report.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let access: Arc<dyn SqlAccess> = Arc::new(FaultAccess {
        inner: fixture.runtime.sql(),
        calls: calls.clone(),
        unknown_at: Some(1),
        fail_reindex: false,
        before_first: None,
    });
    let error = super::fix::TEST_ACTION_ACCESS
        .scope(
            access,
            capture.apply_fixes(&fixture.registry, &fixture.token, report),
        )
        .await
        .unwrap_err();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(error.retryable_failure_context().is_none());
    let wire = crate::runtime_error_value(error, crate::DomainDisposition::NotCommitted);
    assert_eq!(wire["domain_disposition"], "unknown");
    assert_eq!(wire["retryable"], false);
    let detail = &wire["details"]["lint_fix"];
    assert_eq!(detail["outcome"], "unknown");
    assert_eq!(detail["failed_action"]["violation_index"], 1);
    assert_eq!(detail["confirmed_receipts"].as_array().unwrap().len(), 1);
    assert_eq!(detail["report"]["summary"]["fixed"], 1);
    assert_eq!(
        detail["report"]["stats"],
        serde_json::to_value(before.stats).unwrap()
    );
    assert_eq!(
        fixture
            .runtime
            .list_notes(&fixture.token, Some("question"), 10, 0)
            .await
            .unwrap()
            .len(),
        1,
        "unknown action committed but has no confirmed receipt and is never replayed"
    );
    assert_eq!(
        fixture
            .runtime
            .get_entity(&fixture.token, fixture.old.id)
            .await
            .unwrap()
            .properties,
        fixture.old.properties,
        "later action was not attempted"
    );
}

#[tokio::test]
async fn confirmed_statement_failure_rolls_back_and_preserves_native_error_after_prior_commit() {
    let fixture = Fixture::new(runtime()).await;
    let (capture, report) = fixture.capture_report().await;
    execute(&fixture.runtime,"CREATE TRIGGER refuse_lint_question BEFORE INSERT ON notes WHEN NEW.kind='question' BEGIN SELECT RAISE(ABORT,'fixture question failure'); END",vec![]).await;
    let error = capture
        .apply_fixes(&fixture.registry, &fixture.token, report)
        .await
        .unwrap_err();
    assert!(matches!(
        error.refusal_source(),
        crate::RuntimeError::Storage(_)
    ));
    let wire = crate::runtime_error_value(error, crate::DomainDisposition::NotCommitted);
    assert_eq!(wire["details"]["lint_fix"]["outcome"], "failed");
    assert_eq!(
        wire["details"]["lint_fix"]["confirmed_receipts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(wire["domain_disposition"], "unknown");
    assert_eq!(wire["retryable"], false);
    assert!(fixture
        .runtime
        .list_notes(&fixture.token, Some("question"), 10, 0)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        fixture
            .runtime
            .get_entity(&fixture.token, fixture.old.id)
            .await
            .unwrap()
            .properties,
        fixture.old.properties
    );
}

#[derive(Debug)]
struct RefuseDelete;
impl khive_gate::Gate for RefuseDelete {
    fn check(
        &self,
        request: &khive_gate::GateRequest,
    ) -> Result<khive_gate::GateDecision, khive_gate::GateError> {
        Ok(if request.verb == "delete" {
            khive_gate::GateDecision::deny("fixture delete refused")
        } else {
            khive_gate::GateDecision::allow()
        })
    }
}
#[tokio::test]
async fn ordinary_delete_policy_is_failed_not_conflict_and_no_action_is_written() {
    let fixture = Fixture::new(runtime()).await;
    let (capture, report) = fixture.capture_report().await;
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(Arc::new(RefuseDelete));
    let registry = builder.build().unwrap();
    let error = capture
        .apply_fixes(&registry, &fixture.token, report)
        .await
        .unwrap_err();
    assert!(matches!(
        error.refusal_source(),
        crate::RuntimeError::PermissionDenied { .. }
    ));
    let wire = crate::runtime_error_value(error, crate::DomainDisposition::NotCommitted);
    assert_eq!(wire["code"], "permission_denied");
    assert_eq!(wire["domain_disposition"], "unknown");
    assert_eq!(wire["details"]["lint_fix"]["outcome"], "failed");
    assert_eq!(wire["details"]["lint_fix"]["confirmed_receipts"], json!([]));
    assert!(fixture
        .runtime
        .get_edge(&fixture.token, fixture.dangling.id.into())
        .await
        .unwrap()
        .is_some());
    assert!(fixture
        .runtime
        .list_notes(&fixture.token, Some("question"), 10, 0)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn cross_backend_tombstone_is_visible_but_not_a_local_atomic_witness() {
    let first = runtime();
    let second = runtime();
    let token = token(&first);
    let local = entity(&first, "local").await;
    let dead = entity(&second, "remote tombstone").await;
    execute(
        &second,
        "UPDATE entities SET deleted_at=5,version=version+1 WHERE id=?1",
        vec![SqlValue::Text(dead.id.to_string())],
    )
    .await;
    let edge = edge(&first, local.id, dead.id, EdgeRelation::Extends).await;
    let registry = crate::pack::registry_with_lint_owners(
        &first,
        std::collections::HashMap::from([("second".into(), second)]),
    );
    let capture = registry.capture_graph_lint(&first, &token).await.unwrap();
    assert!(capture.edges()[0].target_soft_deleted);
    let report = report(
        vec![violation(
            LintSubject::Edge {
                namespace: "local".into(),
                edge_id: edge.id.into(),
            },
            LintRuleId::DanglingEdge,
            None,
        )],
        &capture,
    );
    let (_, receipts) = capture
        .apply_fixes(&registry, &token, report)
        .await
        .unwrap();
    assert!(matches!(receipts[0], FixReceipt::Unsupported { .. }));
    assert!(first
        .get_edge(&token, edge.id.into())
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn malformed_rows_and_unavailable_target_owners_are_errors() {
    let runtime = runtime();
    let token = token(&runtime);
    let a = entity(&runtime, "source").await;
    let b = entity(&runtime, "target").await;
    let edge = edge(&runtime, a.id, b.id, EdgeRelation::Extends).await;
    execute(
        &runtime,
        "UPDATE graph_edges SET target_backend='not-registered' WHERE id=?1",
        vec![SqlValue::Text(Uuid::from(edge.id).to_string())],
    )
    .await;
    assert!(matches!(
        registry().capture_graph_lint(&runtime, &token).await,
        Err(crate::RuntimeError::Unconfigured(_))
    ));
    execute(
        &runtime,
        "UPDATE graph_edges SET target_backend=NULL,metadata='not JSON' WHERE id=?1",
        vec![SqlValue::Text(Uuid::from(edge.id).to_string())],
    )
    .await;
    assert!(registry()
        .capture_graph_lint(&runtime, &token)
        .await
        .is_err());
}

#[tokio::test]
async fn captured_scope_and_visible_subject_cannot_be_substituted_before_apply() {
    let fixture = Fixture::new(runtime()).await;
    let (capture, report) = fixture.capture_report().await;
    let other = NamespaceToken::mint_authorized(
        Namespace::local(),
        khive_gate::ActorRef::try_new("agent", "different").unwrap(),
    );
    assert!(capture
        .apply_fixes(&fixture.registry, &other, report.clone())
        .await
        .is_err());
    let mut forged = report;
    if let LintSubject::Edge { namespace, .. } = &mut forged.violations[0].subject {
        *namespace = "other".into();
    }
    assert!(capture
        .apply_fixes(&fixture.registry, &fixture.token, forged)
        .await
        .is_err());
    assert!(fixture
        .runtime
        .get_edge(&fixture.token, fixture.dangling.id.into())
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn committed_question_receipt_survives_post_commit_reindex_failure() {
    let fixture = Fixture::new(runtime()).await;
    let (capture, mut report) = fixture.capture_report().await;
    report.violations = vec![report.violations[1].clone()];
    let calls = Arc::new(AtomicUsize::new(0));
    let access: Arc<dyn SqlAccess> = Arc::new(FaultAccess {
        inner: fixture.runtime.sql(),
        calls: calls.clone(),
        unknown_at: None,
        before_first: None,
        fail_reindex: true,
    });
    let error = super::fix::TEST_ACTION_ACCESS
        .scope(
            access,
            capture.apply_fixes(&fixture.registry, &fixture.token, report),
        )
        .await
        .unwrap_err();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        error.refusal_source(),
        crate::RuntimeError::Storage(_)
    ));
    let wire = crate::runtime_error_value(error, crate::DomainDisposition::NotCommitted);
    let detail = &wire["details"]["lint_fix"];
    assert_eq!(detail["outcome"], "failed");
    assert_eq!(detail["report"]["summary"]["fixed"], 1);
    assert_eq!(detail["confirmed_receipts"][0]["status"], "committed");
    let id = Uuid::parse_str(
        detail["confirmed_receipts"][0]["question_note_id"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert!(fixture
        .runtime
        .notes(&fixture.token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .is_some());
    assert_eq!(wire["domain_disposition"], "unknown");
    assert_eq!(wire["retryable"], false);
}

#[tokio::test]
async fn annotation_target_is_resolved_from_separate_events_store() {
    let directory = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let event_path = directory.path().join("events.db");
    let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
        db_path: None,
        events_split: Some(crate::events_split::EventsSplitConfig {
            db_path: event_path.clone(),
            socket_path: None,
        }),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let token = token(&runtime);
    let event = khive_storage::Event::new(
        "local",
        "create",
        khive_types::EventKind::NoteCreated,
        khive_types::SubstrateKind::Note,
        "",
    );
    let sidecar = crate::events_split::direct_backend_for(&event_path).unwrap();
    sidecar
        .events_for_namespace("local")
        .unwrap()
        .append_event(event.clone())
        .await
        .unwrap();
    let note = Note::new("local", "observation", "sidecar annotation");
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    let annotation = edge(&runtime, note.id, event.id, EdgeRelation::Annotates).await;
    let capture = registry()
        .capture_graph_lint(&runtime, &token)
        .await
        .unwrap();
    let inspected = capture
        .edges()
        .iter()
        .find(|item| item.edge.id == annotation.id)
        .unwrap();
    assert!(inspected.source_live && inspected.target_live);
    assert_eq!(inspected.contract_valid, Some(true));
}
