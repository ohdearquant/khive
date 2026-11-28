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

#[tokio::test]
async fn request_id_is_scoped_kept_by_an_empty_inner_scope_and_inherited() {
    assert_eq!(current_request_id(), None);

    let (inner, after_empty, child, after_deadline) = scope_request_id(Some(41), async {
        let inner = scope_request_id(Some(42), async { current_request_id() }).await;
        let after_empty = scope_request_id(None, async { current_request_id() }).await;
        let child = tokio::spawn(inherit_request_read_context(async { current_request_id() }))
            .await
            .expect("child task");
        let after_deadline =
            scope_request_read_deadline(Duration::from_secs(60), async { current_request_id() })
                .await;
        (inner, after_empty, child, after_deadline)
    })
    .await;

    assert_eq!(inner, Some(42), "an inner id replaces the outer one");
    assert_eq!(
        after_empty,
        Some(41),
        "an empty inner scope keeps the outer id"
    );
    assert_eq!(
        child,
        Some(41),
        "a child spawned with the inherited context keeps the id"
    );
    assert_eq!(
        after_deadline,
        Some(41),
        "a nested deadline scope keeps the id"
    );
    assert_eq!(
        tokio::spawn(async { current_request_id() }).await.unwrap(),
        None,
        "a task outside every scope has no id"
    );
}

#[tokio::test]
async fn request_client_cwd_is_scoped_kept_by_an_empty_inner_scope_and_inherited() {
    assert_eq!(current_request_client_cwd(), None);

    let (after_empty, child, inside_request_id_scope) =
        scope_request_client_cwd(Some("/work/a".to_string()), async {
            let after_empty =
                scope_request_client_cwd(None, async { current_request_client_cwd() }).await;
            let child = tokio::spawn(inherit_request_read_context(async {
                current_request_client_cwd()
            }))
            .await
            .expect("child task");
            let inside_request_id_scope =
                scope_request_id(Some(7), async { current_request_client_cwd() }).await;
            (after_empty, child, inside_request_id_scope)
        })
        .await;

    assert_eq!(
        after_empty.as_deref(),
        Some("/work/a"),
        "an empty inner scope keeps the outer value"
    );
    assert_eq!(
        child.as_deref(),
        Some("/work/a"),
        "an inheriting child keeps the value"
    );
    assert_eq!(
        inside_request_id_scope.as_deref(),
        Some("/work/a"),
        "a nested request ID scope keeps the value"
    );
    assert_eq!(
        tokio::spawn(async { current_request_client_cwd() })
            .await
            .unwrap(),
        None,
        "a task outside every scope has no value"
    );
}
