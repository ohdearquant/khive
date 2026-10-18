//! ADR-046 C1–C8: exercise cleanup through the real KG registry and SQL writer.
use std::collections::BTreeMap;
use std::sync::Arc;

use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{EventFilter, SqlAccess, SqlRow, SqlStatement, SqlValue};
use serde_json::{json, Value};
use uuid::Uuid;

const OLD: i64 = 1_000_000;

fn registry(runtime: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("local".into()));
    builder.with_runtime_event_store(runtime).unwrap();
    builder.register(KgPack::new(runtime.clone()));
    builder.build().unwrap()
}

async fn execute(runtime: &KhiveRuntime, sql: &str, params: Vec<SqlValue>) -> u64 {
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement::new(sql, params))
        .await
        .unwrap()
}

async fn projection(runtime: &KhiveRuntime) -> BTreeMap<String, Value> {
    runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement::new(
            "SELECT * FROM proposals_open ORDER BY proposal_id",
            vec![],
        ))
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.text("proposal_id").unwrap().to_owned(),
                serde_json::to_value(row).unwrap(),
            )
        })
        .collect()
}

async fn events(sql: Arc<dyn SqlAccess>) -> BTreeMap<String, Value> {
    sql.reader()
        .await
        .unwrap()
        .query_all(SqlStatement::new(
            "SELECT * FROM events ORDER BY id",
            vec![],
        ))
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            // SqlRow serialization retains the raw SQL TEXT payload spelling, not
            // merely JSON-value equality after decoding/re-encoding an Event.
            (
                row.text("id").unwrap().to_owned(),
                serde_json::to_value(row).unwrap(),
            )
        })
        .collect()
}

fn preserves_prior_events(before: &BTreeMap<String, Value>, after: &BTreeMap<String, Value>) {
    for (id, bytes) in before {
        assert_eq!(
            after.get(id),
            Some(bytes),
            "prior event {id} must be unchanged"
        );
    }
    // Normal dispatch audit may append rows; total event count is not an invariant.
}

async fn proposal_events(runtime: &KhiveRuntime, id: &str) -> BTreeMap<String, Value> {
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .events(&token)
        .unwrap()
        .query_events(
            EventFilter {
                payload_proposal_id: Some(Uuid::parse_str(id).unwrap()),
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|event| (event.id.to_string(), serde_json::to_value(event).unwrap()))
        .collect()
}

async fn resolved(registry: &VerbRegistry, title: &str, decision: &str) -> String {
    let proposal = registry
        .dispatch(
            "propose",
            json!({
                "title": title, "description": "Cleanup acceptance proposal",
                "changeset": {"kind": "add_entity", "entity": {"kind": "concept", "name": title}}
            }),
        )
        .await
        .unwrap();
    let id = proposal["id"].as_str().unwrap().to_owned();
    registry
        .dispatch("review", json!({"id": id, "decision": decision}))
        .await
        .unwrap();
    id
}

async fn age(runtime: &KhiveRuntime, id: &str, timestamp: i64) {
    assert_eq!(
        execute(
            runtime,
            "UPDATE proposals_open SET updated_at=?1 WHERE proposal_id=?2",
            vec![SqlValue::Integer(timestamp), SqlValue::Text(id.into())]
        )
        .await,
        1
    );
}

async fn cleanup(registry: &VerbRegistry) -> Result<Value, RuntimeError> {
    registry
        .dispatch("kg.proposal_cleanup", json!({"older_than": "1d"}))
        .await
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn registry_archives_applied_and_rejected_with_history_identity_and_pagination_intact() {
    for decision in ["approve", "reject"] {
        let runtime = KhiveRuntime::memory().unwrap();
        let registry = registry(&runtime);
        let old = resolved(&registry, "Old resolved proposal", decision).await;
        let recent = resolved(&registry, "Recent resolved proposal", decision).await;
        age(&runtime, &old, OLD).await;
        let before = projection(&runtime).await;
        let expected_status = if decision == "approve" {
            "applied"
        } else {
            "rejected"
        };
        let old_row: SqlRow = serde_json::from_value(before[&old].clone()).unwrap();
        assert_eq!(
            old_row.text("status").unwrap(),
            expected_status,
            "genuine review must resolve the proposal"
        );
        let original = registry.dispatch("get", json!({"id": old})).await.unwrap();
        let history = events(runtime.sql()).await;
        let filtered_history = proposal_events(&runtime, &old).await;
        assert!(filtered_history.len() >= 2);
        assert!(!history.is_empty());
        assert_eq!(cleanup(&registry).await.unwrap(), json!({"archived": 1}));
        let after = projection(&runtime).await;
        assert_eq!(after.len(), before.len());
        for (id, old_value) in before {
            let old_row: SqlRow = serde_json::from_value(old_value).unwrap();
            let new_row: SqlRow = serde_json::from_value(after[&id].clone()).unwrap();
            for column in &old_row.columns {
                if column.name != "archived_at" {
                    assert_eq!(
                        serde_json::to_value(new_row.get(&column.name)).unwrap(),
                        serde_json::to_value(Some(&column.value)).unwrap()
                    );
                }
            }
            assert_eq!(new_row.opt_i64("archived_at").unwrap().is_some(), id == old);
        }
        preserves_prior_events(&history, &events(runtime.sql()).await);
        assert_eq!(proposal_events(&runtime, &old).await, filtered_history);
        for id in history.keys() {
            assert!(
                registry.dispatch("get", json!({"id": id})).await.is_ok(),
                "prior event remains readable by UUID"
            );
        }
        let compact = old.replace('-', "");
        for spelling in [old.as_str(), &compact[..16]] {
            assert_eq!(
                registry
                    .dispatch("get", json!({"id": spelling}))
                    .await
                    .unwrap(),
                original
            );
        }
        let child = registry.dispatch("propose", json!({
            "title": "Archived parent child", "description": "Keep ancestry",
            "parent_id": old, "changeset": {"kind":"add_entity", "entity":{"kind":"concept","name":"Child"}}
        })).await.unwrap();
        assert_eq!(child["parent_id"], old);
        // With only one live resolved proposal, offset=1 must be empty even
        // though the archived row still physically exists after that row.
        for offset in [0, 1] {
            let listed = registry.dispatch("list", json!({"kind":"proposal", "status": expected_status, "actor":"*", "limit":1, "offset":offset})).await.unwrap();
            let items = listed["items"].as_array().unwrap();
            assert_eq!(items.len(), usize::from(offset == 0));
            if offset == 0 {
                assert_eq!(items[0]["id"], recent);
            }
        }
        assert!(registry
            .dispatch("review", json!({"id":old,"decision":"comment"}))
            .await
            .is_err());
        assert!(registry
            .dispatch("withdraw", json!({"id":old}))
            .await
            .is_err());
        assert_eq!(cleanup(&registry).await.unwrap(), json!({"archived":0}));
    }
}

fn file_runtime(
    path: &std::path::Path,
    split: Option<khive_runtime::events_split::EventsSplitConfig>,
) -> KhiveRuntime {
    KhiveRuntime::new_for_test(RuntimeConfig {
        db_path: Some(path.to_path_buf()),
        events_split: split,
        volume_lock_dir: Some(path.parent().unwrap().join("volume-locks")),
        packs: vec!["kg".into()],
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(config_ledger)]
async fn one_statement_rollback_and_retry_work_on_direct_and_queued_writers() {
    for queued in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let runtime = if queued {
            file_runtime(&dir.path().join("kg.db"), None)
        } else {
            KhiveRuntime::memory().unwrap()
        };
        assert_eq!(
            runtime
                .backend()
                .pool()
                .writer_task_handle()
                .unwrap()
                .is_some(),
            queued
        );
        let registry = registry(&runtime);
        for title in ["first", "fault"] {
            let id = resolved(&registry, title, "reject").await;
            age(&runtime, &id, OLD).await;
        }
        let before = projection(&runtime).await;
        let history = events(runtime.sql()).await;
        runtime.sql().writer().await.unwrap().execute_script(
            "CREATE TRIGGER fail_cleanup BEFORE UPDATE OF archived_at ON proposals_open WHEN OLD.title='fault' BEGIN SELECT RAISE(ABORT,'cleanup rollback witness'); END;".into()
        ).await.unwrap();
        let error = cleanup(&registry).await.unwrap_err();
        assert!(matches!(&error, RuntimeError::Storage(_)), "{error:?}");
        assert!(
            error.to_string().contains("cleanup rollback witness"),
            "{error:?}"
        );
        assert_eq!(projection(&runtime).await, before);
        preserves_prior_events(&history, &events(runtime.sql()).await);
        execute(&runtime, "DROP TRIGGER fail_cleanup", vec![]).await;
        assert_eq!(cleanup(&registry).await.unwrap(), json!({"archived":2}));
        assert_eq!(cleanup(&registry).await.unwrap(), json!({"archived":0}));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(config_ledger)]
async fn concurrent_callers_count_each_row_once_on_shared_and_independent_backends() {
    for independent in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("race.db");
        let first = file_runtime(&path, None);
        let a = registry(&first);
        for title in ["one", "two", "three"] {
            let id = resolved(&a, title, "reject").await;
            age(&first, &id, OLD).await;
        }
        let second = if independent {
            file_runtime(&path, None)
        } else {
            first.clone()
        };
        assert_eq!(
            Arc::ptr_eq(&first.backend().pool_arc(), &second.backend().pool_arc()),
            !independent
        );
        let b = registry(&second);
        let (one, two) = tokio::join!(cleanup(&a), cleanup(&b));
        let total =
            one.unwrap()["archived"].as_u64().unwrap() + two.unwrap()["archived"].as_u64().unwrap();
        assert_eq!(total, 3);
        assert_eq!(cleanup(&a).await.unwrap(), json!({"archived":0}));
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn cleanup_preserves_history_and_payload_lookup_when_events_live_in_a_separate_store() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = file_runtime(
        &dir.path().join("kg.db"),
        Some(khive_runtime::events_split::EventsSplitConfig {
            db_path: dir.path().join("events.db"),
            socket_path: None,
        }),
    );
    let registry = registry(&runtime);
    let id = resolved(&registry, "Separate-store history", "reject").await;
    age(&runtime, &id, OLD).await;
    let token = runtime.authorize(Namespace::local()).unwrap();
    let store = runtime.events(&token).unwrap();
    let history = store
        .query_events(
            EventFilter {
                payload_proposal_id: Some(Uuid::parse_str(&id).unwrap()),
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap()
        .items;
    assert!(history.len() >= 2);
    // Model already-separated lifecycle history. Remove the fixture's legacy
    // copies before routing its saved records to the sidecar; the real split
    // store correctly sends legacy-resident retries back to the legacy store.
    // Cleanup itself never moves/deletes/appends these records.
    for event in &history {
        execute(
            &runtime,
            "DELETE FROM events WHERE id=?1",
            vec![SqlValue::Text(event.id.to_string())],
        )
        .await;
    }
    store
        .append_events_idempotent(history.clone())
        .await
        .unwrap();
    let sidecar = runtime.events_sidecar_sql_read_only().unwrap().unwrap();
    let side_before = events(sidecar.clone()).await;
    assert!(history
        .iter()
        .all(|event| side_before.contains_key(&event.id.to_string())));
    let filtered_history = proposal_events(&runtime, &id).await;
    assert_eq!(filtered_history.len(), history.len());
    let before = registry.dispatch("get", json!({"id":id})).await.unwrap();
    assert_eq!(cleanup(&registry).await.unwrap(), json!({"archived":1}));
    preserves_prior_events(&side_before, &events(sidecar).await);
    assert_eq!(proposal_events(&runtime, &id).await, filtered_history);
    assert_eq!(
        registry.dispatch("get", json!({"id":id})).await.unwrap(),
        before
    );
    for event in &history {
        let got = registry
            .dispatch("get", json!({"id":event.id.to_string()}))
            .await
            .unwrap();
        assert_eq!(got["id"], event.id.to_string());
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn a_late_projected_comment_resets_retention_without_reopening_the_proposal() {
    let runtime = KhiveRuntime::memory().unwrap();
    let registry = registry(&runtime);
    let id = resolved(&registry, "Late comment retention", "reject").await;
    age(&runtime, &id, OLD).await;
    let token = runtime.authorize(Namespace::local()).unwrap();
    let payload = khive_types::ProposalReviewedPayload {
        proposal_id: khive_types::Id128::from_u128(Uuid::parse_str(&id).unwrap().as_u128()),
        reviewer: "late reviewer".into(),
        decision: khive_types::ProposalDecision::Comment,
        comment: Some("An already-emitted late comment is being projected".into()),
    };
    let mut event = khive_storage::event::Event::new(
        "local",
        "review",
        khive_types::EventKind::ProposalReviewed,
        khive_storage::SubstrateKind::Entity,
        "late reviewer",
    );
    event.payload = serde_json::to_value(&payload).unwrap();
    event.aggregate_kind = Some("proposal".into());
    event.aggregate_id = Some(Uuid::parse_str(&id).unwrap());
    // Exercise the existing event/projection seam, without relaxing the public
    // terminal-review refusal that the registry fixture above pins.
    khive_pack_kg::projection_worker::ProposalsProjectionWorker::new(runtime.clone())
        .reviewed_and_emit(&token, &payload, event, false)
        .await
        .unwrap();
    let before = projection(&runtime).await;
    let row: SqlRow = serde_json::from_value(before[&id].clone()).unwrap();
    assert_eq!(row.text("status").unwrap(), "rejected");
    assert_eq!(row.text("last_decision").unwrap(), "comment");
    assert!(row.i64("updated_at").unwrap() > OLD);
    let history = events(runtime.sql()).await;
    assert_eq!(cleanup(&registry).await.unwrap(), json!({"archived":0}));
    assert_eq!(projection(&runtime).await, before);
    preserves_prior_events(&history, &events(runtime.sql()).await);
}

#[derive(Debug)]
struct CleanupGate {
    denied: std::sync::atomic::AtomicBool,
    seen: std::sync::Mutex<Vec<khive_gate::GateRequest>>,
}

impl khive_gate::Gate for CleanupGate {
    fn check(
        &self,
        request: &khive_gate::GateRequest,
    ) -> Result<khive_gate::GateDecision, khive_gate::GateError> {
        if request.verb == "kg.proposal_cleanup" {
            self.seen.lock().unwrap().push(request.clone());
            if self.denied.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(khive_gate::GateDecision::deny("cleanup policy witness"));
            }
        }
        Ok(khive_gate::GateDecision::allow())
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn gate_sees_exact_caller_scope_and_args_before_cleanup_writer_admission() {
    let runtime = KhiveRuntime::memory().unwrap();
    for namespace in ["local", "other"] {
        execute(&runtime,
            "INSERT INTO proposals_open (proposal_id,namespace,proposer,title,status,created_at,updated_at) VALUES (?1,?1,'someone else','Gate witness','rejected',1,1)",
            vec![SqlValue::Text(namespace.into())]).await;
    }
    let gate = Arc::new(CleanupGate {
        denied: std::sync::atomic::AtomicBool::new(true),
        seen: std::sync::Mutex::new(vec![]),
    });
    let mut builder = VerbRegistryBuilder::new();
    builder
        .with_actor_id(Some("seat:cleanup".into()))
        .with_gate(gate.clone());
    builder.register(KgPack::new(runtime.clone()));
    // Deliberately omit an audit store so this admission counter observes the
    // cleanup path alone, not a permitted ordinary policy-audit append.
    let registry = builder.build().unwrap();
    let args = json!({"older_than":"1d", "namespace":"other"});
    let before = projection(&runtime).await;
    let admissions = runtime
        .backend()
        .pool()
        .writer_acquisition_snapshot()
        .acquisitions;
    let error = registry
        .dispatch("kg.proposal_cleanup", args.clone())
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::PermissionDenied { .. }));
    assert_eq!(projection(&runtime).await, before);
    assert_eq!(
        runtime
            .backend()
            .pool()
            .writer_acquisition_snapshot()
            .acquisitions,
        admissions
    );
    {
        let seen = gate.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].actor.id, "seat:cleanup");
        assert_eq!(seen[0].namespace.as_str(), "other");
        assert_eq!(seen[0].verb, "kg.proposal_cleanup");
        assert_eq!(seen[0].args, args);
    }
    gate.denied
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        registry
            .dispatch("kg.proposal_cleanup", args)
            .await
            .unwrap(),
        json!({"archived":1})
    );
    assert!(
        runtime
            .backend()
            .pool()
            .writer_acquisition_snapshot()
            .acquisitions
            > admissions
    );
    let after = projection(&runtime).await;
    assert_eq!(after["local"], before["local"]);
    let other: SqlRow = serde_json::from_value(after["other"].clone()).unwrap();
    assert!(other.opt_i64("archived_at").unwrap().is_some());
}
