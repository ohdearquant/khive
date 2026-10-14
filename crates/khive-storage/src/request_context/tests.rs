use super::*;

#[test]
fn read_ceiling_accepts_up_to_one_day_and_corrects_loudly_past_it() {
    let cases: [(Option<&str>, u64, bool); 9] = [
        (None, DEFAULT_REQUEST_READ_TIMEOUT_SECS, false),
        (Some("1"), 1, false),
        (Some("3600"), 3_600, false),
        (Some("7200"), 7_200, false),
        (Some("86400"), MAX_REQUEST_READ_TIMEOUT_SECS, false),
        (Some("86401"), MAX_REQUEST_READ_TIMEOUT_SECS, true),
        (Some("0"), DEFAULT_REQUEST_READ_TIMEOUT_SECS, true),
        (Some("-5"), DEFAULT_REQUEST_READ_TIMEOUT_SECS, true),
        (Some("two hours"), DEFAULT_REQUEST_READ_TIMEOUT_SECS, true),
    ];
    for (raw, seconds, corrected) in cases {
        let (got, correction) = resolve_request_read_timeout_secs(raw);
        assert_eq!(got, seconds, "ceiling for {raw:?}");
        assert_eq!(correction.is_some(), corrected, "correction for {raw:?}");
    }
}

#[tokio::test]
async fn nested_scopes_merge_cancellation_sources() {
    let (outer_tx, outer_rx) = tokio::sync::watch::channel(false);
    let (_inner_tx, inner_rx) = tokio::sync::watch::channel(false);

    let stopped = scope_request_read_cancellation(
        outer_rx,
        scope_request_read_cancellation(inner_rx, async move {
            outer_tx.send(true).expect("outer scope remains live");
            wait_for_request_read_cancellation().await;
            request_read_is_cancelled()
        }),
    )
    .await;

    assert!(stopped);
}

#[tokio::test]
async fn sender_loss_is_request_abandonment() {
    let (tx, rx) = tokio::sync::watch::channel(false);
    drop(tx);

    let stopped =
        scope_request_read_cancellation(rx, async { capture_request_read_context().stop_reason() })
            .await;

    assert_eq!(stopped, Some(RequestReadStopReason::Cancelled));
}

#[tokio::test]
async fn spawned_child_inherits_the_same_cancellation() {
    let (tx, rx) = tokio::sync::watch::channel(false);

    let stopped = scope_request_read_cancellation(rx, async move {
        let child = tokio::spawn(inherit_request_read_context(async {
            wait_for_request_read_cancellation().await;
            request_read_is_cancelled()
        }));
        tx.send(true).expect("child receiver remains live");
        child.await.expect("child task")
    })
    .await;

    assert!(stopped);
}

#[tokio::test]
async fn nested_deadline_keeps_the_earlier_absolute_instant() {
    let earlier = RequestReadDeadline::after(Duration::from_secs(10));
    let later = RequestReadDeadline::after(Duration::from_secs(20));

    let effective =
        scope_request_read_deadline_at(earlier, async { effective_request_read_deadline(later) })
            .await;

    assert_eq!(effective.async_at(), earlier.async_at());
}

#[tokio::test]
async fn async_phase_cannot_degrade_cancelled_work_to_success() {
    let (tx, rx) = tokio::sync::watch::channel(false);
    tx.send(true).expect("receiver remains live");

    let result = scope_request_read_cancellation(rx, async {
        await_request_read_phase("test.phase", async { 7_u8 }).await
    })
    .await;

    assert!(matches!(result, Err(StorageError::Timeout { .. })));
}
