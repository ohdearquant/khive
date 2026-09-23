use crate::atomic_prepare::prepare_op;
use crate::atomic_runner::{run_atomic_unit, AtomicOpFailure, AtomicRunOutcome};
use crate::curation::{ContentMergeStrategy, EntityDedupMergePolicy, EntityPatch};
use crate::{EntityCreateSpec, KhiveRuntime, NamespaceToken};
use khive_storage::{Entity, SqlStatement, SqlValue};
use serde_json::json;

async fn seed(runtime: &KhiveRuntime, token: &NamespaceToken, name: &str) -> Entity {
    let entity = runtime
        .create_entity(token, "concept", None, name, None, None, vec![])
        .await
        .unwrap();
    assert_eq!(entity.version, 1);
    entity
}

async fn events(runtime: &KhiveRuntime) -> i64 {
    let count = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM events".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap()
        .unwrap();
    match count {
        SqlValue::Integer(n) => n,
        other => panic!("COUNT(*) returned {other:?}"),
    }
}

#[tokio::test]
async fn issue2673_writer_checks_caller_version_after_preparation() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let entity = seed(&runtime, &token, "Version race").await;
    let plan = prepare_op(
        &runtime,
        &token,
        "update",
        &json!({"id":entity.id,"name":"loser","expected_version":1}),
    )
    .await
    .unwrap();
    let winner = runtime
        .update_entity(
            &token,
            entity.id,
            EntityPatch {
                name: Some("winner".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(winner.version, 2);
    let events_before = events(&runtime).await;
    let outcome = run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        AtomicRunOutcome::RolledBack {
            failed_op_index: 0,
            failure: AtomicOpFailure::EntityConflict(super::EntityVersionConflict {
                expected: 1,
                current: 2
            })
        }
    ));
    assert_eq!(
        serde_json::to_value(runtime.get_entity(&token, entity.id).await.unwrap()).unwrap(),
        serde_json::to_value(winner).unwrap()
    );
    assert_eq!(events(&runtime).await, events_before);
}

#[tokio::test]
async fn issue2673_atomic_version_conflict_rolls_back_prior_entity_and_audit_writes() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let entity = seed(&runtime, &token, "Atomic version").await;
    let mut plans = Vec::new();
    for name in ["first", "second"] {
        plans.push(
            prepare_op(
                &runtime,
                &token,
                "update",
                &json!({"id":entity.id,"name":name,"expected_version":1}),
            )
            .await
            .unwrap(),
        );
    }
    let events_before = events(&runtime).await;
    let outcome = run_atomic_unit(runtime.sql().as_ref(), plans)
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        AtomicRunOutcome::RolledBack {
            failed_op_index: 1,
            failure: AtomicOpFailure::EntityConflict(super::EntityVersionConflict {
                expected: 1,
                current: 2
            })
        }
    ));
    assert_eq!(
        serde_json::to_value(runtime.get_entity(&token, entity.id).await.unwrap()).unwrap(),
        serde_json::to_value(&entity).unwrap()
    );
    assert_eq!(events(&runtime).await, events_before);
    let current = prepare_op(
        &runtime,
        &token,
        "update",
        &json!({"id":entity.id,"name":"accepted","expected_version":1}),
    )
    .await
    .unwrap();
    assert!(matches!(
        run_atomic_unit(runtime.sql().as_ref(), vec![current])
            .await
            .unwrap(),
        AtomicRunOutcome::Committed { .. }
    ));
    assert_eq!(
        runtime.get_entity(&token, entity.id).await.unwrap().version,
        2
    );
}

#[tokio::test]
async fn issue2673_entity_version_lifecycle_and_import_writers_advance_once() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let entity = seed(&runtime, &token, "Lifecycle").await;
    let (updated, _) = runtime
        .update_entity_with_expected_version_and_embedding_report(
            &token,
            entity.id,
            EntityPatch {
                name: Some("Lifecycle".into()),
                ..Default::default()
            },
            Some(1),
        )
        .await
        .unwrap();
    assert_eq!(
        updated.version, 2,
        "identical entity update retains write behavior"
    );
    let snapshot = updated.clone();
    let admin = runtime
        .update_entity_if_unchanged(
            &token,
            &snapshot,
            EntityPatch {
                properties: Some(json!({"admin":true})),
                ..Default::default()
            },
            &[],
        )
        .await
        .unwrap();
    assert_eq!(admin.version, 3);
    let archive = runtime.export_kg(&token).await.unwrap();
    runtime.import_kg(&archive, &token).await.unwrap();
    assert_eq!(
        runtime.get_entity(&token, entity.id).await.unwrap().version,
        4
    );
    runtime
        .delete_entity(&token, entity.id, false)
        .await
        .unwrap();
    assert_eq!(
        runtime
            .get_entity_including_deleted(&token, entity.id)
            .await
            .unwrap()
            .unwrap()
            .version,
        5
    );
    let (restored, changed) = runtime
        .restore_entity(&token, entity.id)
        .await
        .unwrap()
        .unwrap();
    assert!(changed);
    assert_eq!(restored.version, 6);
    let (still_live, changed) = runtime
        .restore_entity(&token, entity.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!changed);
    assert_eq!(still_live.version, 6);

    let from = seed(&runtime, &token, "Merge source").await;
    runtime
        .merge_entity(
            &token,
            entity.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::PreferInto,
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        runtime.get_entity(&token, entity.id).await.unwrap().version,
        7
    );
    let tombstone = runtime
        .get_entity_including_deleted(&token, from.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tombstone.version, 2);
    assert_eq!(tombstone.merged_into, Some(entity.id));

    let bulk = runtime
        .create_many(
            &token,
            vec![EntityCreateSpec {
                kind: "concept".into(),
                entity_type: None,
                name: "bulk version".into(),
                description: None,
                properties: None,
                tags: vec![],
            }],
        )
        .await
        .unwrap();
    assert_eq!(bulk[0].version, 1);
    assert_eq!(
        runtime
            .get_entity(&token, bulk[0].id)
            .await
            .unwrap()
            .version,
        1
    );
}

#[tokio::test]
async fn issue2673_direct_atomic_merge_tombstone_advances_once() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let into = seed(&runtime, &token, "Direct survivor").await;
    let from = seed(&runtime, &token, "Direct loser").await;
    // Public atomic merge remains inadmissible; the retained internal planner
    // still owns a direct entity UPDATE covered by the version invariant.
    let plan = prepare_op(
        &runtime,
        &token,
        "merge",
        &json!({"into_id":into.id,"from_id":from.id}),
    )
    .await
    .unwrap();
    assert!(matches!(
        run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .unwrap(),
        AtomicRunOutcome::Committed { .. }
    ));
    assert_eq!(
        runtime
            .get_entity_including_deleted(&token, from.id)
            .await
            .unwrap()
            .unwrap()
            .version,
        2
    );
    assert_eq!(
        runtime.get_entity(&token, into.id).await.unwrap().version,
        1,
        "this internal plan does not mutate the survivor row"
    );
}
