//! Selected and forced audit obligations use the existing real batch lane.
use std::num::{NonZeroU8, NonZeroUsize};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use super::finish;
use super::fixture::*;
use crate::audit_batch::{
    AuditBatch, AuditBatchConfig, AuditBatchControl, AuditProducer, AuditTerminalReason,
    PreparedAuditRow,
};
use crate::error::{AuditObligationReason, DispatchError, DomainDisposition};
use crate::usage::{scope, UsageContext, UsageUnit};
use crate::RuntimeError;
use khive_storage::{Event, SubstrateKind};
use khive_types::EventKind;
use serde_json::{json, Value};

fn configured() -> Rig {
    let store = Arc::new(Store::default());
    let control = Arc::new(Control::default());
    let mut builder = builder(super::Reduced, true, control.clone());
    builder.with_event_store(store.clone());
    builder.with_audit_batch_config(AuditBatchConfig {
        max_pending_rows: NonZeroUsize::new(1).unwrap(),
        max_rows_per_generation: NonZeroUsize::new(1).unwrap(),
        max_commit_attempts: NonZeroU8::new(1).unwrap(),
        admission_deadline: Duration::from_millis(20),
        resolution_deadline: Duration::from_secs(1),
        ..AuditBatchConfig::default()
    });
    Rig {
        registry: builder.build().unwrap(),
        store,
        control,
    }
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("audit lane reaches the exact held condition");
}
fn event(verb: &str) -> Event {
    Event::new(
        "local",
        verb,
        EventKind::Audit,
        SubstrateKind::Event,
        "audit-obligation-fixture",
    )
}
type Submission =
    tokio::task::JoinHandle<Result<crate::audit_batch::AuditCommitOutcome, AuditTerminalReason>>;
async fn full_queue(rig: &Rig) -> (Arc<AuditBatch>, Submission, Submission) {
    let batch = rig.registry.audit_batch_handle().unwrap();
    rig.store.hold.store(true, Ordering::SeqCst);
    let occupant_batch = batch.clone();
    let occupant = tokio::spawn(async move {
        occupant_batch
            .submit(PreparedAuditRow {
                event: event("fixture.occupant"),
                producer: AuditProducer::ConfigLocked,
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), rig.store.started.notified())
        .await
        .unwrap();
    let filler_batch = batch.clone();
    let filler = tokio::spawn(async move {
        filler_batch
            .submit(PreparedAuditRow {
                event: event("fixture.filler"),
                producer: AuditProducer::ConfigLocked,
            })
            .await
    });
    until(|| batch.test_snapshot().pending_rows == 1).await;
    (batch, occupant, filler)
}
fn obligation(error: DispatchError, expected: AuditTerminalReason, value: &Value) {
    assert_eq!(
        error.disposition(),
        DomainDisposition::Committed,
        "audit refusal must preserve committed domain disposition"
    );
    let RuntimeError::AuditObligation {
        failure,
        domain_result,
    } = error.into_source()
    else {
        panic!("retained rows remain audit obligations");
    };
    assert_eq!(
        domain_result, *value,
        "canonical handler value must survive obligation failure"
    );
    assert_eq!(failure.reason, AuditObligationReason::Terminal(expected));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn audit_success_rows_kept_rows_remain_obligations() {
    let name="pack::audit_success_rows_cut_tests::obligation_tests::audit_success_rows_kept_rows_remain_obligations";
    let cases = [
        "selected_queue",
        "selected_append",
        "selected_late",
        "heartbeat_queue",
        "heartbeat_append",
        "heartbeat_late",
        "cleanup_queue",
        "cleanup_append",
        "cleanup_late",
    ];
    let Some(case) = isolate(name, &cases) else {
        return;
    };
    let rig = Arc::new(configured());
    let verb = if case.starts_with("cleanup_") {
        CLEANUP
    } else {
        HEARTBEAT
    };
    let forced = !case.starts_with("selected_");
    if forced {
        call(&rig, verb, 0, false).await.unwrap();
    }
    let result = if !forced {
        clean(verb)
    } else if verb == HEARTBEAT {
        json!({"ok":true,"outcome":"failure"})
    } else {
        json!({"ok":true,"deleted":1,"routed_owners_detached":0})
    };
    *rig.control.result.write().unwrap() = Some(result.clone());
    let before = counts(&rig.registry);
    let expected_seq = u64::from(forced);
    if case.ends_with("queue") {
        let (batch, occupant, filler) = full_queue(&rig).await;
        let submitted = batch.test_snapshot().submitted_rows;
        let error = call(&rig, verb, 100, false).await.unwrap_err();
        obligation(error, AuditTerminalReason::QueueAdmissionExhausted, &result);
        assert_eq!(
            batch.test_snapshot().submitted_rows,
            submitted,
            "refused row never enqueued"
        );
        rig.store.release.notify_one();
        occupant.await.unwrap().unwrap();
        filler.await.unwrap().unwrap();
    } else if case.ends_with("append") {
        rig.store.fail.store(true, Ordering::SeqCst);
        obligation(
            call(&rig, verb, 100, false).await.unwrap_err(),
            AuditTerminalReason::StoreFailure,
            &result,
        );
        rig.store.fail.store(false, Ordering::SeqCst);
    } else {
        rig.store.hold.store(true, Ordering::SeqCst);
        let task_rig = rig.clone();
        let task = tokio::spawn(async move { call(&task_rig, verb, 100, false).await });
        tokio::time::timeout(Duration::from_secs(5), rig.store.started.notified())
            .await
            .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        obligation(
            error,
            AuditTerminalReason::ResolutionDeadlineExpired,
            &result,
        );
        let batch = rig.registry.audit_batch_handle().unwrap();
        assert!(
            batch.test_snapshot().in_flight_generation.is_some(),
            "driver must still own the late row"
        );
        // Release only after the caller receipt, inside the driver's three-second bound.
        rig.store.release.notify_one();
        until(|| {
            rig.store
                .rows()
                .iter()
                .any(|row| row.payload["resource"]["request_id"] == 100)
        })
        .await;
        let row = rig
            .store
            .rows()
            .into_iter()
            .find(|row| row.payload["resource"]["request_id"] == 100)
            .unwrap();
        assert_eq!(
            sampling(&row),
            &json!({"every":10,"seq":expected_seq,"selected_by":if forced{"forced"}else{"sample"}}),
            "late commit preserves original sequence and metadata"
        );
    }
    let after = counts(&rig.registry);
    assert_eq!(
        count(&after.2, verb) - count(&before.2, verb),
        u64::from(forced),
        "force decisions count even when persistence fails"
    );
    assert_eq!(allocations().last().unwrap().seq, expected_seq);
    let _ = rig.registry.shutdown_audit_batch().await;
    let next = super::fixture::rig(super::Reduced, true, true);
    call(&next, verb, 200, false).await.unwrap();
    finish(&next).await;
    assert_eq!(
        allocations().last().unwrap().seq,
        expected_seq + 1,
        "failed admission/append must never refund a number"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn audit_success_rows_skips_never_enter_an_injected_failure() {
    let name="pack::audit_success_rows_cut_tests::obligation_tests::audit_success_rows_skips_never_enter_an_injected_failure";
    let Some(case) = isolate(name, &["queue", "append", "late"]) else {
        return;
    };
    let rig = configured();
    call(&rig, HEARTBEAT, 0, false).await.unwrap();
    let batch = rig.registry.audit_batch_handle().unwrap();
    let occupied = if case == "queue" {
        Some(full_queue(&rig).await)
    } else {
        None
    };
    if case == "append" {
        rig.store.fail.store(true, Ordering::SeqCst);
    }
    if case == "late" {
        rig.store.hold.store(true, Ordering::SeqCst);
    }
    let before = batch.test_snapshot();
    let attempts = rig.store.submitted.load(Ordering::Relaxed);
    assert_eq!(
        call(&rig, HEARTBEAT, 1, false).await.unwrap(),
        clean(HEARTBEAT)
    );
    assert_eq!(batch.test_snapshot().submitted_rows, before.submitted_rows);
    assert_eq!(rig.store.submitted.load(Ordering::Relaxed), attempts);
    assert_eq!(count(&counts(&rig.registry).1, HEARTBEAT), 1);
    rig.store.fail.store(false, Ordering::SeqCst);
    rig.store.hold.store(false, Ordering::SeqCst);
    rig.store.release.notify_one();
    if let Some((_, occupant, filler)) = occupied {
        occupant.await.unwrap().unwrap();
        filler.await.unwrap().unwrap();
    }
    finish(&rig).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn audit_success_rows_intercepted_usage_freezes_without_a_row() {
    let name="pack::audit_success_rows_cut_tests::obligation_tests::audit_success_rows_intercepted_usage_freezes_without_a_row";
    let Some(_) = isolate(name, &["run"]) else {
        return;
    };
    let reduced = rig(super::Reduced, true, true);
    let all = rig(super::All, true, true);
    for (index, verb) in OMITTED.into_iter().chain([HEARTBEAT, CLEANUP]).enumerate() {
        if [HEARTBEAT, CLEANUP].contains(&verb) {
            call(&reduced, verb, 1000 + index as u64, true)
                .await
                .unwrap();
        }
        let red = UsageContext::new();
        let old = UsageContext::new();
        scope(red.clone(), call(&reduced, verb, index as u64, true))
            .await
            .unwrap();
        scope(old.clone(), call(&all, verb, index as u64, true))
            .await
            .unwrap();
        red.add(UsageUnit::GraphHops, 777);
        old.add(UsageUnit::GraphHops, 777);
        assert_eq!(
            red.shipping_snapshot(),
            old.shipping_snapshot(),
            "only skipped-success freeze protects reduced usage from late work"
        );
        assert_eq!(red.shipping_snapshot(), Some(json!({"db_round_trips":3})));
        let row = all
            .store
            .rows()
            .into_iter()
            .find(|row| row.payload["resource"]["request_id"] == index as u64)
            .unwrap();
        assert_eq!(
            row.payload["resource"]["units"],
            red.shipping_snapshot().unwrap()
        );
        assert!(!reduced
            .store
            .rows()
            .iter()
            .any(|row| row.payload["resource"]["request_id"] == index as u64));
    }
    let unmeasured = UsageContext::new();
    unmeasured.mark_unmeasured();
    scope(unmeasured.clone(), call(&reduced, "whoami", 99, true))
        .await
        .unwrap();
    assert!(unmeasured.shipping_snapshot().is_none());
    finish(&reduced).await;
    finish(&all).await;
}
