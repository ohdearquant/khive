#[cfg(unix)]
#[tokio::test]
async fn shutdown_counts_and_logs_queued_and_in_flight_batches_as_dropped() {
    // ADR-170 requires shutdown drops to be visible via counters/logs.
    // Both `run_forwarder` shutdown arms must therefore count every
    // batch it loses: the one parked mid-delivery against a hung peer,
    // and every batch still sitting in the queue behind it. Drives
    // `run_forwarder` directly with a local `CancellationToken` (rather
    // than through `EventsSplitClient`, which wires the process-wide
    // `daemon_shutdown_token()` singleton) so cancelling shutdown here
    // cannot leak into other tests sharing the process.
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("hung-shutdown.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    // Accept and hold the connection open without ever reading or
    // replying, so the first delivery blocks in `deliver_batch`'s
    // read_frame until shutdown cancels it.
    let _server = tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            if let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        }
    });

    let (tx, rx) = tokio::sync::mpsc::channel::<QueuedAppend>(8);
    let counters = Arc::new(ForwardingCounters::default());
    let outage_logged = Arc::new(AtomicBool::new(false));
    let shutdown = tokio_util::sync::CancellationToken::new();

    let forwarder = tokio::spawn(run_forwarder(
        socket,
        rx,
        Arc::clone(&counters),
        outage_logged,
        Duration::from_secs(30),
        shutdown.clone(),
    ));

    fn probe_event(tag: &str) -> Event {
        Event::new(
            "test",
            tag,
            khive_types::EventKind::Audit,
            khive_types::SubstrateKind::Event,
            "tester",
        )
    }

    tx.try_send(QueuedAppend::unmetered(
        "test",
        vec![probe_event("in-flight")],
        Arc::clone(&counters),
    ))
    .expect("queue has room for the in-flight batch");
    // No externally observable "delivery started" signal exists short of
    // instrumenting the forwarder; a short sleep reliably lands inside
    // the delivery `select!` arm before shutdown cancels it, given the
    // 30s delivery timeout has no chance to fire first.
    tokio::time::sleep(Duration::from_millis(200)).await;

    tx.try_send(QueuedAppend::unmetered(
        "test",
        vec![probe_event("queued-1"), probe_event("queued-2")],
        Arc::clone(&counters),
    ))
    .expect("queue has room for the first queued batch");
    tx.try_send(QueuedAppend::unmetered(
        "test",
        vec![probe_event("queued-3")],
        Arc::clone(&counters),
    ))
    .expect("queue has room for the second queued batch");

    shutdown.cancel();

    tokio::time::timeout(Duration::from_secs(5), forwarder)
        .await
        .expect("forwarder must exit promptly on shutdown")
        .expect("forwarder task must not panic");

    assert_eq!(
        counters.dropped_batches.load(Ordering::Relaxed),
        3,
        "the in-flight batch and both queued batches must all be counted as dropped"
    );
    assert_eq!(
        counters.dropped_events.load(Ordering::Relaxed),
        4,
        "1 in-flight + 2 + 1 queued events must all be counted as dropped"
    );
    assert_eq!(
        counters.forwarded_batches.load(Ordering::Relaxed),
        0,
        "a hung peer never acknowledges anything in this test"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_during_backoff_drains_queued_batches() {
    // A failed delivery sends the forwarder into `FORWARDER_BACKOFF`
    // before it loops back to `rx.recv()`. Shutdown arriving while it
    // sits in that backoff sleep must drain and count whatever is left
    // in the queue, not just break out silently. Points at a socket path
    // with no listener bound so the very first delivery attempt fails
    // fast (connect refused) rather than needing a hung peer.
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("no-listener.sock");

    let (tx, rx) = tokio::sync::mpsc::channel::<QueuedAppend>(8);
    let counters = Arc::new(ForwardingCounters::default());
    let outage_logged = Arc::new(AtomicBool::new(false));
    let shutdown = tokio_util::sync::CancellationToken::new();

    let forwarder = tokio::spawn(run_forwarder(
        socket,
        rx,
        Arc::clone(&counters),
        outage_logged,
        Duration::from_secs(30),
        shutdown.clone(),
    ));

    fn probe_event(tag: &str) -> Event {
        Event::new(
            "test",
            tag,
            khive_types::EventKind::Audit,
            khive_types::SubstrateKind::Event,
            "tester",
        )
    }

    tx.try_send(QueuedAppend::unmetered(
        "test",
        vec![probe_event("failed-delivery")],
        Arc::clone(&counters),
    ))
    .expect("queue has room for the first batch");

    // Wait for the failed-delivery drop to land, which proves the
    // forwarder has moved on to the `FORWARDER_BACKOFF` sleep.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if counters.dropped_batches.load(Ordering::Relaxed) >= 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "forwarder never dropped the first (unreachable-daemon) batch"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    tx.try_send(QueuedAppend::unmetered(
        "test",
        vec![
            probe_event("queued-during-backoff-1"),
            probe_event("queued-during-backoff-2"),
        ],
        Arc::clone(&counters),
    ))
    .expect("queue has room for the batch queued during backoff");

    shutdown.cancel();

    tokio::time::timeout(Duration::from_secs(5), forwarder)
        .await
        .expect("forwarder must exit promptly on shutdown, not wait out the full backoff")
        .expect("forwarder task must not panic");

    assert_eq!(
        counters.dropped_batches.load(Ordering::Relaxed),
        2,
        "the failed-delivery batch and the batch queued during backoff must both be counted as dropped"
    );
    assert_eq!(
        counters.dropped_events.load(Ordering::Relaxed),
        3,
        "1 failed-delivery + 2 queued-during-backoff events must all be counted as dropped"
    );
    assert_eq!(
        counters.forwarded_batches.load(Ordering::Relaxed),
        0,
        "no listener is bound, so nothing can ever be acknowledged"
    );
}
