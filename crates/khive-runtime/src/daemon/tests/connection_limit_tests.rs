use super::demand_retirement_tests::{dispatcher, lifecycle};
use super::*;
use crate::daemon::load_limits::*;

#[tokio::test]
async fn connections_past_the_cap_get_a_busy_refusal_while_admitted_ones_still_answer() {
    let admission = ConnectionAdmission::new(2);
    let mut admitted = Vec::new();
    for _ in 0..2 {
        let (client, server) = UnixStream::pair().unwrap();
        let permit = admission.try_admit().expect("under the cap");
        let handler = tokio::spawn(async move {
            let _permit = permit;
            handle_conn_with_lifecycle(
                server,
                dispatcher(None),
                None,
                tokio::time::Instant::now() + INITIAL_FRAME_READ_TIMEOUT,
                None,
            )
            .await;
        });
        admitted.push((client, handler));
    }
    let snapshot = admission.snapshot();
    let counts = (snapshot.limit, snapshot.active, snapshot.refused);
    assert_eq!(counts, (2, 2, 0));

    // The connection past the cap gets no permit and a typed refusal.
    let (mut extra_client, mut extra_server) = UnixStream::pair().unwrap();
    let refused = admit_or_refuse_busy(&admission, &mut extra_server, "idle-test").await;
    assert!(
        refused.is_none(),
        "a connection past the cap gets no permit"
    );
    let refusal: DaemonResponseFrame =
        serde_json::from_slice(&read_frame(&mut extra_client).await.unwrap()).unwrap();
    assert!(!refusal.ok);
    assert_eq!(refusal.served_config_id.as_deref(), Some("idle-test"));
    let detail = refusal.error_detail.expect("a typed refusal");
    assert_eq!(detail["kind"], "runtime");
    assert_eq!(detail["code"], "daemon_busy");
    assert_eq!(detail["limit"], 2);
    assert_eq!(detail["domain_disposition"], "not_committed");
    // The refused connection is closed once the frame has been read.
    drop(extra_server);
    assert!(read_frame(&mut extra_client).await.is_err());
    let snapshot = admission.snapshot();
    let counts = (snapshot.limit, snapshot.active, snapshot.refused);
    assert_eq!(counts, (2, 2, 1));

    // Connections admitted before the refusal still answer.
    for (client, _) in &mut admitted {
        let mut frame = base_request_frame("idle-test");
        frame.probe_only = true;
        write_frame(client, &serde_json::to_vec(&frame).unwrap())
            .await
            .unwrap();
        let response: DaemonResponseFrame =
            serde_json::from_slice(&read_frame(client).await.unwrap()).unwrap();
        assert!(response.ok, "an admitted connection still answers");
    }
    for (_, handler) in admitted {
        handler.await.unwrap();
    }
    let snapshot = admission.snapshot();
    assert_eq!((snapshot.active, snapshot.refused), (0, 1));
    assert!(
        admission.try_admit().is_ok(),
        "ended connections free their slots"
    );
}

#[tokio::test]
async fn metrics_frame_reports_connection_cap_and_recall_ledger_state() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let state = lifecycle(DaemonLifetime::Persistent);
    let handle = tokio::spawn(handle_conn_with_lifecycle(
        server,
        dispatcher(None),
        None,
        tokio::time::Instant::now() + INITIAL_FRAME_READ_TIMEOUT,
        Some(Arc::clone(&state)),
    ));
    let mut frame = base_request_frame("idle-test");
    frame.metrics_only = true;
    write_frame(&mut client, &serde_json::to_vec(&frame).unwrap())
        .await
        .unwrap();
    let response: DaemonResponseFrame =
        serde_json::from_slice(&read_frame(&mut client).await.unwrap()).unwrap();
    handle.await.unwrap();
    let metrics = response
        .metrics
        .expect("a metrics frame carries a snapshot");
    assert_eq!(metrics.connections, Some(state.connections.snapshot()));
    assert!(metrics
        .recall_ledger
        .is_some_and(|ledger| ledger.max_pending > 0));
}

#[tokio::test(start_paused = true)]
#[serial(background_tasks)]
async fn recall_ledger_tasks_stop_at_the_limit_and_a_stalled_one_ends_at_its_timeout() {
    let timeout = std::time::Duration::from_secs(5);
    let bound = Arc::new(RecallLedgerBound::new(2, timeout));
    // A held writer: the ledger work cannot complete on its own.
    let mut held = Vec::new();
    for _ in 0..2 {
        let task = bound.try_spawn(std::future::pending::<()>());
        held.push(task.expect("a ledger task under the limit starts"));
    }
    assert_eq!(bound.snapshot().pending, 2);

    // Past the limit nothing starts, and the skipped write is counted.
    assert!(bound.try_spawn(std::future::pending::<()>()).is_none());
    let snapshot = bound.snapshot();
    let counts = (snapshot.pending, snapshot.skipped, snapshot.timed_out);
    assert_eq!(counts, (2, 1, 0));

    // The paused clock moves only while every task is idle, so the guard
    // below fires only if the tasks' own timeout did not end them first.
    for task in held {
        tokio::time::timeout(timeout * 2, task)
            .await
            .expect("a stalled ledger task ends at its own timeout")
            .unwrap();
    }
    let snapshot = bound.snapshot();
    let counts = (snapshot.pending, snapshot.skipped, snapshot.timed_out);
    assert_eq!(counts, (0, 1, 2));
    assert_eq!(snapshot.timeout_ms, 5_000);

    // Ended tasks give their slots back.
    let done = bound.try_spawn(async {}).expect("a slot is free again");
    done.await.unwrap();
    assert_eq!(bound.snapshot().pending, 0);
}

#[test]
fn limit_parser_accepts_positive_integers_and_rejects_everything_else() {
    assert_eq!(parse_positive_limit(Some("7")), Some(7));
    assert_eq!(parse_positive_limit(Some(" 42 ")), Some(42));
    assert_eq!(parse_positive_limit(None), None);
    for raw in ["", "  ", "0", "-3", "1.5", "many", "18446744073709551616"] {
        assert_eq!(parse_positive_limit(Some(raw)), None, "{raw:?}");
    }
}

#[test]
fn effective_cap_is_reduced_to_the_descriptor_limit_and_never_below_one() {
    use CapSource::{Builtin, Configured, DescriptorLimit};
    // The reserve is 192, so a soft limit of 256 leaves room for 64.
    let table = [
        (None, None, 512, Builtin),
        (Some(100), None, 100, Configured),
        (Some(0), None, 1, Configured),
        (None, Some(256), 64, DescriptorLimit),
        (Some(100), Some(256), 64, DescriptorLimit),
        (Some(10), Some(256), 10, Configured),
        (None, Some(1024), 512, Builtin),
        (Some(900), Some(1024), 832, DescriptorLimit),
        (None, Some(1_048_576), 512, Builtin),
        (Some(2000), Some(1_048_576), 2000, Configured),
        (None, Some(u64::MAX), 512, Builtin),
        (None, Some(193), 1, DescriptorLimit),
        (None, Some(192), 1, DescriptorLimit),
        (None, Some(100), 1, DescriptorLimit),
        (None, Some(0), 1, DescriptorLimit),
    ];
    for (configured, soft, want_cap, want_source) in table {
        let got = effective_connection_cap(configured, soft);
        assert_eq!(
            got,
            (want_cap, want_source),
            "configured={configured:?} soft={soft:?}"
        );
    }
}

#[tokio::test]
async fn startup_cap_follows_the_injected_descriptor_limit() {
    // A soft limit of 256 forces the cap to 64 and reports what was asked for.
    let admission = ConnectionAdmission::from_limits(None, Some(256));
    let snapshot = admission.snapshot();
    assert_eq!((snapshot.limit, snapshot.configured), (64, Some(512)));
    let wire = serde_json::to_value(snapshot).unwrap();
    assert_eq!(wire["configured"], 512);

    // Where the limit leaves room, or cannot be read, nothing is reduced
    // and no configured value is reported.
    for (configured, soft, want_limit) in [(None, Some(1024), 512), (Some(7), None, 7)] {
        let admission = ConnectionAdmission::from_limits(configured, soft);
        let snapshot = admission.snapshot();
        assert_eq!((snapshot.limit, snapshot.configured), (want_limit, None));
        let wire = serde_json::to_value(snapshot).unwrap();
        assert!(wire.get("configured").is_none());
    }

    // The enforced cap admits exactly that many connections and the busy
    // refusal carries it as its limit.
    let mut held = Vec::new();
    for _ in 0..64 {
        held.push(admission.try_admit().expect("under the cap"));
    }
    let (mut client, mut server) = UnixStream::pair().unwrap();
    let refused = admit_or_refuse_busy(&admission, &mut server, "idle-test").await;
    assert!(refused.is_none(), "the 65th connection gets no permit");
    let refusal: DaemonResponseFrame =
        serde_json::from_slice(&read_frame(&mut client).await.unwrap()).unwrap();
    assert_eq!(refusal.error_detail.expect("a typed refusal")["limit"], 64);
}

#[test]
fn soft_nofile_limit_reads_this_process_limit() {
    assert!(soft_nofile_limit().is_some_and(|soft| soft > 0));
}
