use super::*;

fn channel_handle(
    capacity: usize,
) -> (
    WriterTaskHandle,
    mpsc::Receiver<Box<dyn AnyWriteRequest + Send>>,
) {
    let (tx, rx) = mpsc::channel(capacity);
    (
        WriterTaskHandle {
            tx,
            backend_key: None,
            db: "admission-test".into(),
            slow_write_threshold: None,
            enqueue_timeout: Duration::from_millis(20),
            admission_metrics: WriterAdmissionMetrics::new(),
        },
        rx,
    )
}

#[tokio::test]
async fn capacity_cancellation_and_accepted_abandonment_have_distinct_lifetimes() {
    let (handle, mut rx) = channel_handle(1);
    let first_reply = handle.enqueue(|_| Ok::<_, StorageError>(())).await.unwrap();
    let mut waiting = Box::pin(handle.enqueue(|_| Ok::<_, StorageError>(())));
    std::future::poll_fn(|cx| {
        assert!(waiting.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let pending = handle.admission_snapshot().unwrap();
    assert_eq!(pending.queue_depth, 2);
    assert_eq!(pending.queue_depth_high_water, 2);
    assert_eq!(pending.admission_wait.count, 1);
    drop(waiting);
    assert_eq!(handle.admission_snapshot().unwrap().queue_depth, 1);
    drop(first_reply);
    assert_eq!(handle.admission_snapshot().unwrap().queue_depth, 1);
    let request = rx.recv().await.unwrap();
    assert_eq!(handle.admission_snapshot().unwrap().admission_wait.count, 1);
    request.reply_error(writer_task_terminated(WriterTaskRequestState::NotStarted));
    let settled = handle.admission_snapshot().unwrap();
    assert_eq!(settled.queue_depth, 0);
    assert_eq!(settled.saturation_total, 0);
    assert_eq!(settled.accepted_queue_age.count, 0);
    assert_eq!(settled.service.count, 0);
}

#[tokio::test]
async fn accepted_timestamp_is_published_before_same_task_consumer_runs() {
    for top_level in [false, true] {
        let (handle, mut rx) = channel_handle(1);
        let mut sender: std::pin::Pin<Box<dyn Future<Output = Result<(), StorageError>> + '_>> =
            if top_level {
                Box::pin(handle.send_top_level(|_| Ok::<_, StorageError>(())))
            } else {
                Box::pin(handle.send(|_| Ok::<_, StorageError>(())))
            };
        std::future::poll_fn(|cx| {
            assert!(sender.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        // The sender stays suspended at its reply await while this task consumes.
        let mut request = rx
            .try_recv()
            .expect("sender published before waiting for reply");
        let accepted = handle.admission_snapshot().unwrap();
        assert_eq!(accepted.admission_wait.count, 1);
        assert_eq!(accepted.queue_depth, 1);
        sealed::Sealed::start_execution(request.as_mut());
        let executing = handle.admission_snapshot().unwrap();
        assert_eq!(executing.queue_depth, 0);
        assert_eq!(executing.accepted_queue_age.count, 1);
        drop(sender);
        request.reply_error(writer_task_terminated(WriterTaskRequestState::NotStarted));
        let complete = handle.admission_snapshot().unwrap();
        assert_eq!(
            complete.service.count, 1,
            "dequeued refusal is executed, even after caller abandonment"
        );
        assert_eq!(complete.saturation_total, 0);
    }
}

#[tokio::test]
async fn every_typed_reply_branch_records_one_service_sample() {
    for case in [
        "wrapped",
        "top_success",
        "top_open",
        "top_panic",
        "begin_error",
    ] {
        let (handle, mut rx) = channel_handle(1);
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE witness(value INTEGER)")
            .unwrap();
        let reply = handle
            .enqueue_inner(
                move |conn| {
                    if case == "top_panic" {
                        panic!("intentional top-level reply witness");
                    }
                    if case == "top_open" {
                        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
                    }
                    conn.execute("INSERT INTO witness VALUES (1)", []).unwrap();
                    Ok::<_, StorageError>(())
                },
                case.starts_with("top_"),
                false,
                false,
                SubmitLabels::None,
            )
            .await
            .unwrap();
        let mut request = rx.recv().await.unwrap();
        sealed::Sealed::start_execution(request.as_mut());
        if case == "begin_error" {
            request.reply_error(writer_task_terminated(WriterTaskRequestState::NotStarted));
        } else if case.starts_with("top_") {
            request.execute_and_reply_top_level(&conn);
        } else {
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            request.execute_and_reply(&conn);
        }
        let result = reply.await.unwrap();
        if matches!(case, "top_panic" | "top_open") {
            assert_writer_task_terminal_state(result, WriterTaskRequestState::SideEffectsUnknown);
        } else if case == "begin_error" {
            assert_writer_task_terminal_state(result, WriterTaskRequestState::NotStarted);
        } else {
            result.unwrap();
        }
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM witness", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            rows,
            i64::from(!matches!(case, "top_panic" | "begin_error")),
            "{case}"
        );
        let snapshot = handle.admission_snapshot().unwrap();
        assert_eq!(snapshot.queue_depth, 0, "{case}");
        assert_eq!(snapshot.admission_wait.count, 1, "{case}");
        assert_eq!(snapshot.accepted_queue_age.count, 1, "{case}");
        assert_eq!(snapshot.service.count, 1, "{case}");
        assert_eq!(snapshot.saturation_total, 0, "{case}");
        if !conn.is_autocommit() {
            conn.execute_batch("ROLLBACK").unwrap();
        }
    }
}

#[tokio::test]
async fn real_channel_phase_samples_fit_independently_observed_intervals() {
    let (handle, mut rx) = channel_handle(1);
    let first_reply = handle.enqueue(|_| Ok::<_, StorageError>(())).await.unwrap();
    let baseline = handle.admission_snapshot().unwrap();
    let before_offer = Instant::now();
    let mut waiting = Box::pin(handle.enqueue(|_| Ok::<_, StorageError>(())));
    std::future::poll_fn(|cx| {
        assert!(waiting.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let after_offer = Instant::now();
    let first = rx.recv().await.unwrap();
    first.reply_error(writer_task_terminated(WriterTaskRequestState::NotStarted));
    assert_writer_task_terminal_state(
        first_reply.await.unwrap(),
        WriterTaskRequestState::NotStarted,
    );
    let before_accept = Instant::now();
    let reply = waiting.await.unwrap();
    let after_accept = Instant::now();
    let accepted = handle.admission_snapshot().unwrap();
    let admission = accepted.admission_wait.sum - baseline.admission_wait.sum;
    assert!(admission >= before_accept.duration_since(after_offer));
    assert!(admission <= after_accept.duration_since(before_offer));
    let mut request = rx.recv().await.unwrap();
    let before_start = Instant::now();
    sealed::Sealed::start_execution(request.as_mut());
    let after_start = Instant::now();
    let age = handle.admission_snapshot().unwrap().accepted_queue_age.sum;
    assert!(age >= before_start.duration_since(after_accept));
    assert!(age <= after_start.duration_since(before_accept));
    let before_reply = Instant::now();
    request.reply_error(writer_task_terminated(WriterTaskRequestState::NotStarted));
    let after_reply = Instant::now();
    assert_writer_task_terminal_state(reply.await.unwrap(), WriterTaskRequestState::NotStarted);
    let settled = handle.admission_snapshot().unwrap();
    assert!(settled.service.sum >= before_reply.duration_since(after_start));
    assert!(settled.service.sum <= after_reply.duration_since(before_start));
    assert_eq!(settled.admission_wait.count, 2);
    assert_eq!(settled.accepted_queue_age.count, 1);
    assert_eq!(settled.service.count, 1);
}

#[tokio::test]
async fn disabled_telemetry_does_not_change_committed_storage_result() {
    for reason in [Unavailable::Poisoned, Unavailable::CounterOverflow] {
        let directory = tempfile::tempdir().unwrap();
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(directory.path().join("disabled-metrics.db")),
            volume_lock_dir: Some(directory.path().join("volume-locks")),
            ..PoolConfig::for_test()
        })
        .unwrap();
        pool.writer()
            .unwrap()
            .conn()
            .execute_batch("CREATE TABLE witness(value INTEGER)")
            .unwrap();
        let handle = spawn(&pool, 1).unwrap();
        // Module tests cause actual poison/overflow; here inject the resulting disabled state.
        handle.admission_metrics.disable_for_test(reason);
        let result = handle
            .send(|conn| {
                conn.execute("INSERT INTO witness VALUES (42)", [])
                    .map_err(|error| StorageError::Internal(error.to_string()))
            })
            .await
            .unwrap();
        assert_eq!(result, 1);
        assert_eq!(handle.admission_snapshot().unwrap_err(), reason);
        let stored: i64 = pool
            .reader()
            .unwrap()
            .query_row("SELECT value FROM witness", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stored, 42);
    }
}
