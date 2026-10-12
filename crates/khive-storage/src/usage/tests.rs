use super::*;

#[tokio::test]
async fn count_is_noop_without_scope_and_counts_inside() {
    count(UsageUnit::EmbedCalls, 3);
    let ctx = UsageContext::new();
    scope(ctx.clone(), async {
        count(UsageUnit::EmbedCalls, 2);
        count(UsageUnit::FtsPasses, 1);
        count(UsageUnit::GraphHops, 0);
    })
    .await;
    let snap = ctx.snapshot();
    assert_eq!(snap["embed_calls"], 2);
    assert_eq!(snap["fts_passes"], 1);
    assert!(
        snap.get("graph_hops").is_none(),
        "zero counters are omitted"
    );
}

#[tokio::test]
async fn joined_spawned_child_counts_via_explicit_handle() {
    let ctx = UsageContext::new();
    scope(ctx.clone(), async {
        let handle = current().expect("scope armed");
        let child = tokio::spawn(scope(handle, async {
            count(UsageUnit::VectorPasses, 2);
        }));
        child.await.expect("join child");
    })
    .await;
    assert_eq!(ctx.snapshot()["vector_passes"], 2);
}

#[tokio::test]
async fn detached_spawn_without_handle_contributes_nothing() {
    let ctx = UsageContext::new();
    scope(ctx.clone(), async {
        // A detached task spawned WITHOUT re-entering the scope: its
        // counts must not reach the dispatch context.
        let orphan = tokio::spawn(async {
            count(UsageUnit::EmbedCalls, 99);
        });
        orphan.await.expect("join orphan");
    })
    .await;
    assert_eq!(
        ctx.snapshot(),
        serde_json::json!({}),
        "task-locals do not cross tokio::spawn; only an explicit handle propagates"
    );
}

#[test]
fn saturating_add_never_wraps() {
    let ctx = UsageContext::new();
    ctx.add(UsageUnit::EventRows, u64::MAX);
    ctx.add(UsageUnit::EventRows, 5);
    assert_eq!(ctx.snapshot()["event_rows"], u64::MAX);
}

#[test]
fn unmeasured_mark_wins_after_freeze_and_later_increments() {
    let ctx = UsageContext::new();
    ctx.add(UsageUnit::EventRows, 2);
    let frozen = ctx.freeze();
    assert_eq!(ctx.shipping_snapshot(), Some(frozen.clone()));

    ctx.clone().mark_unmeasured();
    assert_eq!(ctx.shipping_snapshot(), None);
    ctx.add(UsageUnit::EventRows, 3);
    ctx.add(UsageUnit::EmbedCalls, 1);
    assert_eq!(ctx.shipping_snapshot(), None);
    assert_eq!(ctx.freeze(), frozen);
    assert_eq!(ctx.frozen_or_snapshot(), frozen);
    assert_eq!(ctx.snapshot()["event_rows"], 5);
}

#[test]
fn unmeasured_mark_before_freeze_preserves_internal_readers() {
    let ctx = UsageContext::new();
    assert_eq!(ctx.shipping_snapshot(), Some(serde_json::json!({})));
    ctx.mark_unmeasured();
    ctx.mark_unmeasured();
    ctx.add(UsageUnit::DbRoundTrips, 1);
    assert_eq!(ctx.shipping_snapshot(), None);
    assert_eq!(ctx.freeze(), serde_json::json!({"db_round_trips": 1}));
    assert_eq!(ctx.shipping_snapshot(), None);
}
#[tokio::test]
async fn native_evidence_preserves_unknown_event_accounting() {
    for terminal in [false, true] {
        let context = UsageContext::new();
        let error = if terminal {
            crate::StorageError::writer_task_terminated(
                crate::WriterTaskRequestState::SideEffectsUnknown,
            )
        } else {
            crate::StorageError::WriterTaskRequestFailed {
                request_state: crate::WriterTaskRequestState::SideEffectsUnknown,
                source: Box::new(crate::StorageError::Internal("unsettled".into())),
            }
        }
        .with_sqlite_write_failure(crate::error::SqliteWriteFailure {
            stage: crate::error::SqliteWriteStage::Commit,
            primary_code: 10,
            extended_code: 1034,
            settlement_unknown: false,
        });
        scope(context.clone(), async {
            account_event_write(Ok(1));
            account_event_write(Err(&error));
        })
        .await;
        assert_eq!(context.snapshot()["event_rows"], 1);
        assert!(context.shipping_snapshot().is_none());
    }
}
