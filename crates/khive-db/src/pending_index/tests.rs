use super::*;
use std::any::Any;

use khive_storage::{BoxFuture, SqlAccess};

type TestUnit<T> =
    Box<dyn for<'w> FnOnce(&'w mut dyn SqlWriter) -> BoxFuture<'w, StorageResult<T>> + Send>;

async fn unit<T: Send + 'static>(bridge: &crate::SqlBridge, work: TestUnit<T>) -> StorageResult<T> {
    let value = bridge
        .atomic_unit(Box::new(move |writer| {
            Box::pin(async move { Ok(Box::new(work(writer).await?) as Box<dyn Any + Send>) })
        }))
        .await?;
    Ok(*value.downcast::<T>().expect("test unit result type"))
}

async fn fixture() -> (crate::SqlBridge, Uuid) {
    let backend = crate::StorageBackend::memory().unwrap();
    backend.prepare_core_schema().unwrap();
    let bridge = crate::SqlBridge::new(backend.pool_arc(), false);
    let id = Uuid::new_v4();
    unit(&bridge, Box::new(move |writer| Box::pin(async move {
        writer.execute(SqlStatement::new(
            "INSERT INTO entities(id, namespace, kind, name, created_at, updated_at) VALUES (?1, 'local', 'concept', 'before', 1, 1)",
            vec![SqlValue::Text(id.to_string())],
        )).await?;
        Ok(())
    }))).await.unwrap();
    (bridge, id)
}

async fn advance(writer: &mut dyn SqlWriter, id: Uuid) -> StorageResult<()> {
    assert_eq!(writer.execute(SqlStatement::new(
        "UPDATE entities SET name='after', updated_at=updated_at+1, version=version+1 WHERE id=?1",
        vec![SqlValue::Text(id.to_string())],
    )).await?, 1);
    Ok(())
}

async fn snapshot(bridge: &crate::SqlBridge, id: Uuid) -> Option<PendingEntityIndex> {
    read(&mut *bridge.reader().await.unwrap(), id)
        .await
        .unwrap()
}

fn vector(model: &str, dimensions: usize) -> PendingIndexTargetKind {
    PendingIndexTargetKind::Vector {
        model: model.to_owned(),
        dimensions: vec![dimensions],
    }
}

fn target<'a>(
    record: &'a PendingEntityIndex,
    kind: &PendingIndexTargetKind,
) -> &'a PendingIndexTarget {
    record
        .targets
        .iter()
        .find(|target| target.kind == *kind)
        .unwrap()
}

#[tokio::test]
async fn enqueue_failure_rolls_back_entity_replacement() {
    let (bridge, id) = fixture().await;
    bridge.writer().await.unwrap().execute_script(
        "CREATE TRIGGER reject_pending_index BEFORE INSERT ON pending_entity_indexes BEGIN SELECT RAISE(ABORT, 'injected pending failure'); END;".into(),
    ).await.unwrap();
    let result = unit(
        &bridge,
        Box::new(move |writer| {
            Box::pin(async move {
                advance(writer, id).await?;
                enqueue(writer, id, "local", 2, &[], Uuid::new_v4(), 10).await
            })
        }),
    )
    .await;
    assert!(result.is_err());
    let mut reader = bridge.reader().await.unwrap();
    let row = reader
        .query_row(SqlStatement::new(
            "SELECT name, version FROM entities WHERE id=?1",
            vec![SqlValue::Text(id.to_string())],
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.text("name").unwrap(), "before");
    assert_eq!(row.i64("version").unwrap(), 1);
    assert!(read(&mut *reader, id).await.unwrap().is_none());
}

#[tokio::test]
async fn coalescing_preserves_unfinished_models_and_retry_history() {
    let (bridge, id) = fixture().await;
    let first = unit(
        &bridge,
        Box::new(move |writer| {
            Box::pin(async move {
                advance(writer, id).await?;
                let record = enqueue(
                    writer,
                    id,
                    "local",
                    2,
                    &[("old".into(), 3), ("done".into(), 3)],
                    Uuid::new_v4(),
                    10,
                )
                .await?;
                assert!(
                    record_failure(
                        writer,
                        id,
                        record.generation,
                        &vector("old", 3),
                        "vector_write",
                        90
                    )
                    .await?
                );
                assert!(
                    record_failure(
                        writer,
                        id,
                        record.generation,
                        &PendingIndexTargetKind::Fts,
                        "fts_write",
                        80
                    )
                    .await?
                );
                assert!(finish(writer, id, record.generation, &PendingIndexTargetKind::Fts).await?);
                assert!(finish(writer, id, record.generation, &vector("done", 3)).await?);
                Ok(record)
            })
        }),
    )
    .await
    .unwrap();
    let second = unit(
        &bridge,
        Box::new(move |writer| {
            Box::pin(async move {
                advance(writer, id).await?;
                enqueue(
                    writer,
                    id,
                    "local",
                    3,
                    &[("new".into(), 4)],
                    Uuid::new_v4(),
                    20,
                )
                .await
            })
        }),
    )
    .await
    .unwrap();
    assert_ne!(first.generation, second.generation);
    assert_eq!(second.first_pending_at, 10);
    assert_eq!(second.source_version, 3);
    assert_eq!(second.targets.len(), 3);
    assert!(second.targets.iter().all(|target| !target.done));
    let old = target(&second, &vector("old", 3));
    assert_eq!(
        (
            old.attempts,
            old.last_error.as_deref(),
            old.next_eligible_at
        ),
        (1, Some("vector_write"), 90)
    );
    let fts = target(&second, &PendingIndexTargetKind::Fts);
    assert_eq!(
        (
            fts.attempts,
            fts.last_error.as_deref(),
            fts.next_eligible_at
        ),
        (1, Some("fts_write"), 80)
    );
    let new = target(&second, &vector("new", 4));
    assert_eq!(
        (
            new.attempts,
            new.last_error.as_deref(),
            new.next_eligible_at
        ),
        (0, None, 20)
    );
    assert_eq!(snapshot(&bridge, id).await, Some(second));
}

#[tokio::test]
async fn dimension_conflicts_retain_both_definitions_and_cannot_finish() {
    let (bridge, id) = fixture().await;
    let conflict = unit(
        &bridge,
        Box::new(move |writer| {
            Box::pin(async move {
                advance(writer, id).await?;
                let first = enqueue(
                    writer,
                    id,
                    "local",
                    2,
                    &[("model".into(), 3)],
                    Uuid::new_v4(),
                    10,
                )
                .await?;
                assert!(
                    record_failure(
                        writer,
                        id,
                        first.generation,
                        &vector("model", 3),
                        "model_missing",
                        90
                    )
                    .await?
                );
                advance(writer, id).await?;
                let next = enqueue(
                    writer,
                    id,
                    "local",
                    3,
                    &[("model".into(), 4), ("model".into(), 4)],
                    Uuid::new_v4(),
                    20,
                )
                .await?;
                let kind = PendingIndexTargetKind::Vector {
                    model: "model".into(),
                    dimensions: vec![3, 4],
                };
                assert!(kind.is_blocked());
                assert!(!finish(writer, id, next.generation, &kind).await?);
                assert!(!finish(writer, id, next.generation, &vector("model", 4)).await?);
                assert!(
                    !record_failure(
                        writer,
                        id,
                        first.generation,
                        &vector("model", 3),
                        "vector_write",
                        21
                    )
                    .await?
                );
                Ok(next)
            })
        }),
    )
    .await
    .unwrap();
    let blocked = &conflict.targets[1];
    assert_eq!(
        (
            blocked.attempts,
            blocked.last_error.as_deref(),
            blocked.next_eligible_at
        ),
        (1, Some("model_missing"), 90)
    );
    assert_eq!(snapshot(&bridge, id).await, Some(conflict));
}

#[tokio::test]
async fn recaptured_completed_definition_keeps_retry_history_in_either_snapshot_order() {
    for dimensions in [[3, 4], [4, 3]] {
        let (bridge, id) = fixture().await;
        unit(
            &bridge,
            Box::new(move |writer| {
                Box::pin(async move {
                    advance(writer, id).await?;
                    let first = enqueue(
                        writer,
                        id,
                        "local",
                        2,
                        &[("model".into(), 3)],
                        Uuid::new_v4(),
                        10,
                    )
                    .await?;
                    assert!(
                        record_failure(
                            writer,
                            id,
                            first.generation,
                            &vector("model", 3),
                            "vector_write",
                            90
                        )
                        .await?
                    );
                    assert!(finish(writer, id, first.generation, &vector("model", 3)).await?);
                    advance(writer, id).await?;
                    let models = dimensions
                        .into_iter()
                        .map(|dimension| ("model".into(), dimension))
                        .collect::<Vec<_>>();
                    let next = enqueue(writer, id, "local", 3, &models, Uuid::new_v4(), 20).await?;
                    let blocked = &next.targets[1];
                    assert_eq!(
                        blocked.kind,
                        PendingIndexTargetKind::Vector {
                            model: "model".into(),
                            dimensions: vec![3, 4]
                        }
                    );
                    assert_eq!(
                        (
                            blocked.attempts,
                            blocked.last_error.as_deref(),
                            blocked.next_eligible_at
                        ),
                        (1, Some("vector_write"), 90)
                    );
                    assert!(!blocked.done);
                    Ok(())
                })
            }),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn completion_is_generation_guarded_and_removes_only_the_final_target() {
    let (bridge, id) = fixture().await;
    let first_generation = unit(
        &bridge,
        Box::new(move |writer| {
            Box::pin(async move {
                advance(writer, id).await?;
                let record = enqueue(
                    writer,
                    id,
                    "local",
                    2,
                    &[("model".into(), 3)],
                    Uuid::new_v4(),
                    10,
                )
                .await?;
                assert!(finish(writer, id, record.generation, &PendingIndexTargetKind::Fts).await?);
                assert!(
                    !finish(writer, id, record.generation, &PendingIndexTargetKind::Fts).await?
                );
                assert!(
                    !record_failure(
                        writer,
                        id,
                        record.generation,
                        &PendingIndexTargetKind::Fts,
                        "fts_write",
                        11
                    )
                    .await?
                );
                assert!(read(writer, id).await?.is_some());
                assert!(finish(writer, id, record.generation, &vector("model", 3)).await?);
                assert!(read(writer, id).await?.is_none());
                Ok(record.generation)
            })
        }),
    )
    .await
    .unwrap();
    assert!(snapshot(&bridge, id).await.is_none());
    unit(
        &bridge,
        Box::new(move |writer| {
            Box::pin(async move {
                advance(writer, id).await?;
                let new = enqueue(writer, id, "local", 3, &[], Uuid::new_v4(), 20).await?;
                assert!(!finish(writer, id, first_generation, &PendingIndexTargetKind::Fts).await?);
                assert!(
                    !record_failure(
                        writer,
                        id,
                        first_generation,
                        &PendingIndexTargetKind::Fts,
                        "fts_write",
                        21
                    )
                    .await?
                );
                assert!(!remove(writer, id, first_generation).await?);
                assert_eq!(read(writer, id).await?, Some(new));
                Ok(())
            })
        }),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn live_guard_refuses_changed_version_namespace_deleted_and_merged_sources() {
    let (bridge, id) = fixture().await;
    unit(
        &bridge,
        Box::new(move |writer| {
            Box::pin(async move {
                advance(writer, id).await?;
                let record = enqueue(writer, id, "local", 2, &[], Uuid::new_v4(), 10).await?;
                assert!(live_matches(writer, &record).await?);
                advance(writer, id).await?;
                assert!(!live_matches(writer, &record).await?);
                let mut current = record.clone();
                current.source_version = 3;
                assert!(live_matches(writer, &current).await?);
                current.namespace = "other".into();
                assert!(!live_matches(writer, &current).await?);
                current.namespace = "local".into();
                writer
                    .execute(SqlStatement::new(
                        "UPDATE entities SET deleted_at=4, version=version+1 WHERE id=?1",
                        vec![SqlValue::Text(id.to_string())],
                    ))
                    .await?;
                current.source_version = 4;
                assert!(!live_matches(writer, &current).await?);
                writer.execute(SqlStatement::new(
            "UPDATE entities SET deleted_at=NULL, merged_into=?2, version=version+1 WHERE id=?1",
            vec![SqlValue::Text(id.to_string()), SqlValue::Text(Uuid::new_v4().to_string())],
        )).await?;
                current.source_version = 5;
                assert!(!live_matches(writer, &current).await?);
                assert_eq!(read(writer, id).await?, Some(record));
                Ok(())
            })
        }),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn failure_metadata_rejects_raw_messages_saturates_attempts_and_preserves_delay() {
    let (bridge, id) = fixture().await;
    unit(
        &bridge,
        Box::new(move |writer| {
            Box::pin(async move {
                advance(writer, id).await?;
                let mut record = enqueue(writer, id, "local", 2, &[], Uuid::new_v4(), 10).await?;
                assert!(record_failure(
                    writer,
                    id,
                    record.generation,
                    &PendingIndexTargetKind::Fts,
                    "message with entity text",
                    11
                )
                .await
                .is_err());
                record.targets[0].attempts = u64::MAX;
                assert!(replace(writer, record.generation, &record).await?);
                assert!(
                    record_failure(
                        writer,
                        id,
                        record.generation,
                        &PendingIndexTargetKind::Fts,
                        "fts_write",
                        12
                    )
                    .await?
                );
                assert!(
                    record_failure(
                        writer,
                        id,
                        record.generation,
                        &PendingIndexTargetKind::Fts,
                        "fts_write",
                        11
                    )
                    .await?
                );
                let updated = read(writer, id).await?.unwrap();
                assert_eq!(updated.targets[0].attempts, u64::MAX);
                assert_eq!(updated.targets[0].last_error.as_deref(), Some("fts_write"));
                assert_eq!(updated.targets[0].next_eligible_at, 12);
                Ok(())
            })
        }),
    )
    .await
    .unwrap();
}

fn namespace_move_record() -> PendingEntityIndex {
    PendingEntityIndex {
        entity_id: Uuid::from_u128(0x2222),
        namespace: "stale".into(),
        source_version: 2,
        generation: Uuid::from_u128(0x1111),
        first_pending_at: 10,
        targets: vec![
            PendingIndexTarget {
                kind: PendingIndexTargetKind::Fts,
                done: true,
                attempts: 2,
                last_error: Some("old_failure".into()),
                next_eligible_at: 9_000_000_000_000_000,
            },
            PendingIndexTarget {
                kind: vector("completed", 3),
                done: true,
                attempts: 4,
                last_error: None,
                next_eligible_at: 90,
            },
            PendingIndexTarget {
                kind: PendingIndexTargetKind::Vector {
                    model: "blocked".into(),
                    dimensions: vec![3, 5],
                },
                done: false,
                attempts: 7,
                last_error: Some("model_unavailable".into()),
                next_eligible_at: 9_000_000_000_000_001,
            },
        ],
    }
}

#[test]
fn namespace_move_generation_has_an_independent_binary_golden() {
    let old = Uuid::from_u128(0x1111);
    let entity = Uuid::from_u128(0x2222);
    let generation = move_generation(old, entity, "destination/α", 7).unwrap();
    assert_eq!(
        generation.to_string(),
        "4ec303c0-17df-5d4c-ba53-d5fecf914cd2"
    );
    assert_ne!(generation, old);
    for different in [
        move_generation(old, entity, "destination/α", 8).unwrap(),
        move_generation(old, entity, "destination/α\0", 7).unwrap(),
        move_generation(old, Uuid::from_u128(0x2223), "destination/α", 7).unwrap(),
        move_generation(Uuid::from_u128(0x1112), entity, "destination/α", 7).unwrap(),
    ] {
        assert_ne!(generation, different);
    }
    assert!(move_generation(Uuid::nil(), entity, "target", 7).is_err());
    assert!(move_generation(old, entity, "target", 0).is_err());
    assert!(move_generation(old, entity, "target", -1).is_err());
}

#[test]
fn namespace_move_refresh_preserves_complete_targets_and_retry_state() {
    let old = namespace_move_record();
    let before = serde_json::to_vec(&old).unwrap();
    let generation = move_generation(old.generation, old.entity_id, "target", 10).unwrap();
    let refreshed = refresh_for_live_revision(&old, "target", 10, generation).unwrap();
    let mut expected = old.clone();
    expected.namespace = "target".into();
    expected.source_version = 10;
    expected.generation = generation;
    for target in &mut expected.targets {
        target.done = false;
    }
    assert_eq!(refreshed, expected);
    assert_eq!(serde_json::to_vec(&old).unwrap(), before);
    assert_eq!(refreshed.targets.len(), 3);
    assert!(refreshed.targets.iter().all(|target| !target.done));
    assert!(refreshed.targets[2].kind.is_blocked());
}

#[test]
fn namespace_move_refresh_refuses_reused_generation_and_invalid_revision() {
    let old = namespace_move_record();
    for generation in [Uuid::nil(), old.generation] {
        assert!(refresh_for_live_revision(&old, "target", 10, generation).is_err());
    }
    let generation = move_generation(old.generation, old.entity_id, "target", 10).unwrap();
    for version in [0, -1] {
        assert!(refresh_for_live_revision(&old, "target", version, generation).is_err());
    }
    let mut invalid = old.clone();
    invalid.targets[2].done = true;
    assert!(refresh_for_live_revision(&invalid, "target", 10, generation).is_err());
}
