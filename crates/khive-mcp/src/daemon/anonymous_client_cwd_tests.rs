//! The client working directory on daemon frames reaches the audit row of an anonymous caller
//! only (khive-gate `docs/api/audit-events.md`, "Client working directory").

use std::time::Duration;

use super::test_harness::{clear_daemon_env, connect_when_ready, exchange};
use super::*;
use khive_runtime::daemon::run_daemon;
use khive_runtime::{KhiveRuntime, Namespace};
use khive_storage::{Event, EventFilter, PageRequest};
use khive_types::EventKind;
use serde_json::Value;
use serial_test::serial;

const CWD: &str = "/home/dev/scratch/probe";

fn kg_runtime() -> KhiveRuntime {
    let mut config = KhiveRuntime::memory()
        .expect("memory runtime")
        .config()
        .clone();
    config.default_namespace = Namespace::local();
    config.packs = vec!["kg".to_string(), "comm".to_string()];
    config.actor_id = None;
    KhiveRuntime::new(config).expect("in-memory runtime")
}

fn stats_frame(
    config_id: &str,
    actor_id: Option<&str>,
    client_cwd: Option<&str>,
) -> DaemonRequestFrame {
    DaemonRequestFrame {
        ops: "stats()".to_string(),
        namespace: "local".to_string(),
        actor_id: actor_id.map(str::to_string),
        client_cwd: client_cwd.map(str::to_string),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        from_wire: true,
        ..Default::default()
    }
}

/// Serve `frames` in order from one daemon over `runtime`, asserting each is served.
async fn serve(runtime: &KhiveRuntime, frames: &[(Option<&str>, Option<&str>)]) {
    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    let server = crate::server::KhiveMcpServer::new(runtime.clone()).expect("server builds");
    let config_id = server.config_id().to_string();
    let handle = tokio::spawn(async move {
        let _ = run_daemon(server).await;
    });
    drop(connect_when_ready(&sock).await);
    for (actor, cwd) in frames {
        let response = exchange(&sock, &stats_frame(&config_id, *actor, *cwd)).await;
        assert!(
            response.ok,
            "frame {actor:?}/{cwd:?} must be served: {response:?}"
        );
    }
    handle.abort();
    let _ = handle.await;
    clear_daemon_env();
}

/// The `stats` audit rows, once `expected` of them are stored (the append may trail the reply).
async fn stats_rows(runtime: &KhiveRuntime, expected: usize) -> Vec<Event> {
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let events = runtime.events(&token).expect("event store");
    for _ in 0..100 {
        let rows: Vec<Event> = events
            .query_events(
                EventFilter::default(),
                PageRequest {
                    offset: 0,
                    limit: 500,
                },
            )
            .await
            .expect("event page")
            .items
            .into_iter()
            .filter(|row| row.kind == EventKind::Audit && row.verb == "stats")
            .collect();
        if rows.len() >= expected {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("fewer than {expected} stats audit rows were stored");
}

fn is_anonymous(row: &Event) -> bool {
    row.actor.starts_with("anonymous:")
}

#[tokio::test]
#[serial]
async fn an_anonymous_call_stores_its_client_directory() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let runtime = kg_runtime();
    serve(&runtime, &[(None, Some(CWD))]).await;
    let rows = stats_rows(&runtime, 1).await;
    let row = rows
        .iter()
        .find(|row| is_anonymous(row))
        .expect("the anonymous row");
    assert_eq!(row.payload["client_cwd"]["path"], CWD, "{}", row.payload);
    assert_eq!(row.payload["client_cwd"]["cut"], false, "{}", row.payload);
}

#[tokio::test]
#[serial]
async fn an_attributed_call_from_the_same_directory_stores_no_path() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let runtime = kg_runtime();
    // The anonymous frame beside it is the control: the same daemon and directory do store it.
    serve(
        &runtime,
        &[(Some("agent:peer"), Some(CWD)), (None, Some(CWD))],
    )
    .await;
    let rows = stats_rows(&runtime, 2).await;
    let attributed = rows
        .iter()
        .find(|row| !is_anonymous(row))
        .expect("the attributed row");
    assert!(
        attributed.payload.get("client_cwd").is_none(),
        "an attributed row never carries the directory: {}",
        attributed.payload
    );
    let anonymous = rows
        .iter()
        .find(|row| is_anonymous(row))
        .expect("the anonymous control row");
    assert_eq!(
        anonymous.payload["client_cwd"]["path"], CWD,
        "{}",
        anonymous.payload
    );
}

#[tokio::test]
#[serial]
async fn a_frame_without_the_field_still_writes_its_row() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let runtime = kg_runtime();
    // `client_cwd: None` is skipped when serialized, so the wire frame is the older shape.
    let wire = serde_json::to_value(stats_frame("cfg", None, None)).expect("frame json");
    assert!(wire.get("client_cwd").is_none(), "{wire}");
    serve(&runtime, &[(None, None)]).await;
    let rows = stats_rows(&runtime, 1).await;
    let row = rows
        .iter()
        .find(|row| is_anonymous(row))
        .expect("the anonymous row");
    assert!(row.payload.get("client_cwd").is_none(), "{}", row.payload);
    assert!(matches!(row.payload, Value::Object(_)), "{}", row.payload);
}
