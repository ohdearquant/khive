use super::*;
use std::future::{poll_fn, Future};
use std::task::Poll;

const UNITS: [(UsageUnit, &str); 7] = [
    (UsageUnit::EmbedCalls, "embed_calls"),
    (UsageUnit::FtsPasses, "fts_passes"),
    (UsageUnit::VectorPasses, "vector_passes"),
    (UsageUnit::GraphHops, "graph_hops"),
    (UsageUnit::DbRoundTrips, "db_round_trips"),
    (UsageUnit::AnnJobsConsumed, "ann_jobs_consumed"),
    (UsageUnit::EventRows, "event_rows"),
];

#[tokio::test]
async fn nested_dispatch_merges_each_frozen_counter_once_and_restores_parent() {
    let parent = UsageContext::new();
    scope(parent.clone(), async {
        for (unit, _) in UNITS {
            count(unit, 1);
        }
        let child = scope_child(async {
            for (unit, _) in UNITS {
                count(unit, 2);
            }
            scope_child(async {
                for (unit, _) in UNITS {
                    count(unit, 4);
                }
                current().unwrap().freeze();
            })
            .await;
            let child = current().unwrap();
            child.freeze();
            for (unit, _) in UNITS {
                count(unit, 100);
            }
            child
        })
        .await;
        for (unit, key) in UNITS {
            assert_eq!(child.shipping_snapshot().unwrap()[key], 6);
            assert_eq!(child.snapshot()[key], 106, "late work actually happened");
            assert_eq!(parent.shipping_snapshot().unwrap()[key], 7);
            count(unit, 8);
        }
    })
    .await;
    for (_, key) in UNITS {
        assert_eq!(parent.shipping_snapshot().unwrap()[key], 15);
    }
    assert!(current().is_none());
}

#[tokio::test]
async fn nested_dispatch_preserves_error_and_saturates_unfrozen_work() {
    let parent = UsageContext::new();
    parent.add(UsageUnit::EmbedCalls, u64::MAX - 1);
    let result = scope(
        parent.clone(),
        scope_child(async {
            count(UsageUnit::EmbedCalls, 3);
            count(UsageUnit::DbRoundTrips, 2);
            Err::<(), _>("nested dispatch failed")
        }),
    )
    .await;
    assert_eq!(result, Err("nested dispatch failed"));
    assert_eq!(
        parent.shipping_snapshot(),
        Some(serde_json::json!({"embed_calls":u64::MAX,"db_round_trips":2}))
    );
}

#[tokio::test]
async fn nested_dispatch_unknown_completeness_propagates_even_after_freeze() {
    for freeze_first in [false, true] {
        let parent = UsageContext::new();
        scope(parent.clone(), async {
            count(UsageUnit::EmbedCalls, 1);
            scope_child(async {
                let child = current().unwrap();
                count(UsageUnit::EmbedCalls, 2);
                if freeze_first {
                    child.freeze();
                }
                child.mark_unmeasured();
            })
            .await;
            scope_child(async { count(UsageUnit::EmbedCalls, 4) }).await;
        })
        .await;
        assert!(parent.shipping_snapshot().is_none());
        assert_eq!(parent.snapshot()["embed_calls"], 5);
    }
    let parent = UsageContext::new();
    parent.freeze();
    scope(
        parent.clone(),
        scope_child(async { current().unwrap().mark_unmeasured() }),
    )
    .await;
    assert!(parent.shipping_snapshot().is_none());
}

#[tokio::test]
async fn interrupted_nested_dispatch_marks_parent_unmeasured() {
    let parent = UsageContext::new();
    scope(parent.clone(), async {
        let mut child = Box::pin(scope_child(async {
            count(UsageUnit::EmbedCalls, 1);
            std::future::pending::<()>().await;
        }));
        poll_fn(|cx| {
            assert!(child.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert!(parent.shipping_snapshot().is_some());
        drop(child);
        assert!(parent.shipping_snapshot().is_none());
        count(UsageUnit::FtsPasses, 2);
    })
    .await;
    assert_eq!(parent.snapshot()["fts_passes"], 2);
}

#[tokio::test]
async fn panicking_nested_dispatch_marks_parent_unmeasured() {
    let parent = UsageContext::new();
    let result = tokio::spawn(scope(
        parent.clone(),
        scope_child(async {
            count(UsageUnit::EmbedCalls, 1);
            panic!("nested fixture panic");
        }),
    ))
    .await;
    assert!(result.unwrap_err().is_panic());
    assert!(parent.shipping_snapshot().is_none());
}

#[tokio::test]
async fn nested_dispatch_does_not_arm_an_unmeasured_caller() {
    assert!(current().is_none());
    let result = scope_child(async {
        assert!(current().is_none());
        count(UsageUnit::EmbedCalls, 7);
        Err::<(), _>("unchanged")
    })
    .await;
    assert_eq!(result, Err("unchanged"));
    assert!(current().is_none());
}

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
