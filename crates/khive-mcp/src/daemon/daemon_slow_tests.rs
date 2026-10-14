//! The named outcomes of a daemon that is alive but does not answer in time,
//! and of the other post-write failures (ADR-049 Amendment 14 §3).

use super::test_harness::{clear_daemon_env, connect_when_ready};
use super::*;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use serial_test::serial;

fn diagnostics() -> serde_json::Value {
    serde_json::to_value(bridge_diagnostics_snapshot().expect("diagnostics snapshot")).unwrap()
}

#[test]
#[serial]
fn post_write_failures_carry_distinct_reasons_and_an_unknown_disposition() {
    reset_fallback_counters();

    let slow = post_write_error(PostWriteFailure::Slow { elapsed_ms: 1234 })
        .data
        .expect("daemon_slow carries data");
    assert_eq!(slow["reason"], "daemon_slow");
    assert_eq!(slow["phase"], "after_write");
    assert_eq!(slow["elapsed_ms"], 1234);
    assert_eq!(slow["domain_disposition"], "unknown");

    let lost = post_write_error(PostWriteFailure::ConnectionLost)
        .data
        .expect("connection loss carries data");
    assert_eq!(lost["reason"], "daemon_connection_lost");
    assert_eq!(lost["domain_disposition"], "unknown");
    assert!(lost.get("phase").is_none());

    let unreadable = post_write_error(PostWriteFailure::ParseFailure)
        .data
        .expect("parse failure carries data");
    assert_eq!(unreadable["reason"], "parse_failure");
    assert_eq!(unreadable["domain_disposition"], "unknown");

    let snapshot = diagnostics();
    assert_eq!(snapshot["daemon_slow"]["after_write"], 1);
    assert_eq!(snapshot["daemon_slow"]["before_dispatch"], 0);
    assert_eq!(
        snapshot["fallback_total"], 0,
        "a refusal is never counted as a fallback"
    );
    reset_fallback_counters();
}

#[test]
#[serial]
fn a_boot_fence_probe_timeout_is_a_not_committed_daemon_slow_refusal() {
    reset_fallback_counters();

    let refusal = daemon_slow_before_dispatch_error(500)
        .data
        .expect("daemon_slow carries data");
    assert_eq!(refusal["reason"], "daemon_slow");
    assert_eq!(refusal["phase"], "before_dispatch");
    assert_eq!(refusal["elapsed_ms"], 500);
    assert_eq!(refusal["domain_disposition"], "not_committed");
    assert!(
        refusal.get(STRICT_FALLBACK_MARKER).is_none(),
        "daemon_slow is a refusal of its own, not a rejected fallback"
    );

    let snapshot = diagnostics();
    assert_eq!(snapshot["daemon_slow"]["before_dispatch"], 1);
    assert_eq!(snapshot["daemon_slow"]["after_write"], 0);
    assert_eq!(snapshot["fallback_total"], 0);
    reset_fallback_counters();
}

#[test]
fn every_post_write_outcome_maps_to_its_failure() {
    assert_eq!(
        PostWriteFailure::from_outcome(&ForwardOutcome::ResponseTimeout { elapsed_ms: 7 }),
        Some(PostWriteFailure::Slow { elapsed_ms: 7 })
    );
    assert_eq!(
        PostWriteFailure::from_outcome(&ForwardOutcome::ResponseLost),
        Some(PostWriteFailure::ConnectionLost)
    );
    assert_eq!(
        PostWriteFailure::from_outcome(&ForwardOutcome::ParseFailure),
        Some(PostWriteFailure::ParseFailure)
    );
    assert_eq!(
        PostWriteFailure::from_outcome(&ForwardOutcome::NoSocket),
        None
    );
}

/// A real daemon serving a file-backed store, busy behind a write lock held
/// by another connection: the client's read deadline passes after its frame
/// was written, so the caller sees `daemon_slow` with phase `after_write`.
#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn a_daemon_busy_on_a_held_write_lock_answers_daemon_slow_after_write() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    clear_daemon_env();
    reset_fallback_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    std::env::set_var("KHIVE_REQUEST_READ_TIMEOUT_SECS", "2");

    let db = dir.path().join("busy.db");
    let config = RuntimeConfig {
        db_path: Some(db.clone()),
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..Default::default()
    };
    let server =
        crate::server::KhiveMcpServer::new(KhiveRuntime::new(config).expect("file runtime"))
            .expect("server");
    let config_id = server.config_id().to_string();
    let daemon = tokio::spawn(async move {
        let _ = khive_runtime::daemon::run_daemon(server).await;
    });
    drop(connect_when_ready(&sock).await);

    let blocker = rusqlite::Connection::open(&db).expect("open the store from outside");
    blocker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("hold the store's write lock");

    let frame = DaemonRequestFrame {
        plan: false,
        ops: "create(kind=\"observation\", content=\"written behind a held lock\")".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.clone(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };
    let never_spawn = || -> std::io::Result<std::process::Child> {
        panic!("a live daemon is on the socket; nothing may be spawned")
    };
    let started = std::time::Instant::now();
    let out = forward_or_spawn_with(&frame, &never_spawn).await;
    let waited = started.elapsed();

    blocker.execute_batch("ROLLBACK").expect("release the lock");
    let error = match out {
        Some(Err(error)) => error,
        other => panic!("a write stuck behind a held lock must time out, got {other:?}"),
    };
    let data = error.data.expect("daemon_slow carries data");
    assert_eq!(data["reason"], "daemon_slow", "{data}");
    assert_eq!(data["phase"], "after_write", "{data}");
    assert_eq!(data["domain_disposition"], "unknown", "{data}");
    let elapsed_ms = data["elapsed_ms"].as_u64().expect("elapsed_ms");
    assert!(
        (1_500..=u64::try_from(waited.as_millis()).unwrap()).contains(&elapsed_ms),
        "elapsed_ms {elapsed_ms} must be the response wait (deadline 2 s, call took {waited:?})"
    );
    let snapshot = diagnostics();
    assert_eq!(snapshot["daemon_slow"]["after_write"], 1);
    assert_eq!(snapshot["fallback_total"], 0);

    // The daemon stays up once the lock is released.
    let probe = DaemonRequestFrame {
        ops: "stats()".to_string(),
        ..frame
    };
    let answered = forward_or_spawn_with(&probe, &never_spawn).await;
    assert!(
        matches!(answered, Some(Ok(_))),
        "the daemon answers after the lock is released: {answered:?}"
    );

    daemon.abort();
    std::env::remove_var("KHIVE_REQUEST_READ_TIMEOUT_SECS");
    clear_daemon_env();
    reset_fallback_counters();
}
