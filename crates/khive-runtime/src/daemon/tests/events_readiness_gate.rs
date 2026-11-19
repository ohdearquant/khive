use super::*;

fn gated(
    window: std::time::Duration,
) -> (
    GatedDispatch,
    crate::events_split::EventsReadinessSignal,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    let inner = demand_retirement_tests::dispatcher(None);
    let calls = Arc::clone(&inner.dispatch_calls);
    let (signal, readiness) = crate::events_split::events_readiness(
        PathBuf::from("/tmp/gate-test.db.events.sock"),
        window,
    );
    (GatedDispatch { inner, readiness }, signal, calls)
}

async fn send(
    dispatcher: GatedDispatch,
    frame: DaemonRequestFrame,
) -> (UnixStream, tokio::task::JoinHandle<()>) {
    let (mut client, server) = UnixStream::pair().unwrap();
    let handle = tokio::spawn(handle_conn_with_lifecycle(
        server,
        dispatcher,
        None,
        tokio::time::Instant::now() + INITIAL_FRAME_READ_TIMEOUT,
        None,
    ));
    write_frame(&mut client, &serde_json::to_vec(&frame).unwrap())
        .await
        .unwrap();
    (client, handle)
}

#[tokio::test]
async fn request_waits_for_the_events_daemon_then_dispatches() {
    let (dispatcher, signal, calls) = gated(std::time::Duration::from_secs(30));
    let mut frame = base_request_frame("idle-test");
    frame.ops = "stats()".to_owned();
    let (mut client, handle) = send(dispatcher, frame).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "nothing may dispatch before the events daemon answers"
    );
    assert!(!handle.is_finished());
    signal.mark_ready();
    let response: DaemonResponseFrame =
        serde_json::from_slice(&read_frame(&mut client).await.unwrap()).unwrap();
    assert!(response.ok, "{:?}", response.error);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    handle.await.unwrap();
}

#[tokio::test]
async fn request_is_refused_uncommitted_when_the_window_elapses() {
    let (dispatcher, _signal, calls) = gated(std::time::Duration::from_millis(100));
    let mut frame = base_request_frame("idle-test");
    frame.ops = "create(kind=\"note\", content=\"x\")".to_owned();
    frame.request_id = Some(7);
    let (mut client, handle) = send(dispatcher, frame).await;
    let refusal: DaemonResponseFrame =
        serde_json::from_slice(&read_frame(&mut client).await.unwrap()).unwrap();
    assert!(!refusal.ok);
    assert_eq!(refusal.request_id, Some(7));
    let detail = refusal.error_detail.expect("refusal carries detail");
    assert_eq!(detail["code"], "events_daemon_unavailable");
    assert_eq!(detail["retryable"], true);
    assert_eq!(detail["socket"], "/tmp/gate-test.db.events.sock");
    assert_eq!(
        detail["domain_disposition"],
        crate::DomainDisposition::NotCommitted.as_str()
    );
    assert!(refusal
        .error
        .expect("refusal carries a message")
        .contains("/tmp/gate-test.db.events.sock"));
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a refused request must not reach dispatch"
    );
    handle.await.unwrap();
}

#[tokio::test]
async fn probe_frames_do_not_wait_at_the_gate() {
    let (dispatcher, _signal, calls) = gated(std::time::Duration::from_secs(30));
    let mut frame = base_request_frame("idle-test");
    frame.probe_only = true;
    let (mut client, handle) = send(dispatcher, frame).await;
    let response: DaemonResponseFrame =
        tokio::time::timeout(std::time::Duration::from_secs(5), read_frame(&mut client))
            .await
            .expect("a probe must be answered while the gate is closed")
            .map(|raw| serde_json::from_slice(&raw).unwrap())
            .unwrap();
    assert!(response.ok);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    handle.await.unwrap();
}
