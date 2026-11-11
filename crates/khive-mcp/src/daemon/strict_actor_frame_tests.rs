//! Strict attributed-actor mode on daemon request frames: a warm daemon
//! serves each frame under the frame's own actor, so the strict check runs
//! against that actor, not only against the daemon's configured one.

use super::test_harness::{clear_daemon_env, connect_when_ready, exchange};
use super::*;
use khive_runtime::daemon::run_daemon;
use khive_runtime::{KhiveRuntime, Namespace};
use serial_test::serial;

const STRICT_ENV: &str = "KHIVE_REQUIRE_ATTRIBUTED_ACTOR";

fn comm_server(require_attributed: bool) -> crate::server::KhiveMcpServer {
    let mut config = KhiveRuntime::memory()
        .expect("memory runtime")
        .config()
        .clone();
    config.default_namespace = Namespace::parse("test").unwrap();
    config.packs = vec!["kg".to_string(), "comm".to_string()];
    config.actor_id = Some("agent:owner".to_string());
    config.require_attributed_actor = require_attributed;
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
    crate::server::KhiveMcpServer::new(runtime).expect("server builds with kg+comm")
}

fn inbox_frame(config_id: &str, actor_id: Option<&str>) -> DaemonRequestFrame {
    DaemonRequestFrame {
        owned_namespaces: Vec::new(),
        plan: false,
        ops: "comm.inbox(limit=5)".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: actor_id.map(str::to_string),
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: true,
        request_id: None,
    }
}

/// Run `server` as a daemon on a private socket and send one `comm.inbox`
/// frame per actor, in order. Returns the responses.
async fn inbox_responses(
    server: crate::server::KhiveMcpServer,
    actors: &[Option<&str>],
) -> Vec<DaemonResponseFrame> {
    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    let config_id = server.config_id().to_string();
    let handle = tokio::spawn(async move {
        let _ = run_daemon(server).await;
    });
    drop(connect_when_ready(&sock).await);
    let mut responses = Vec::with_capacity(actors.len());
    for actor in actors {
        responses.push(exchange(&sock, &inbox_frame(&config_id, *actor)).await);
    }
    handle.abort();
    let _ = handle.await;
    clear_daemon_env();
    responses
}

fn assert_refused(response: &DaemonResponseFrame, actor: Option<&str>) {
    assert!(
        !response.ok,
        "actor {actor:?} must be refused: {response:?}"
    );
    assert!(response.result.is_none(), "no verb result: {response:?}");
    assert!(!response.config_mismatch, "{response:?}");
    let detail = response.error_detail.as_ref().expect("refusal detail");
    assert_eq!(detail["reason"], "anonymous-actor", "{detail}");
    assert_eq!(detail["code"], "anonymous_actor", "{detail}");
    assert_eq!(
        detail["domain_disposition"],
        khive_runtime::DomainDisposition::NotCommitted.as_str(),
        "{detail}"
    );
}

fn assert_served(response: &DaemonResponseFrame, actor: Option<&str>) {
    assert!(response.ok, "actor {actor:?} must be served: {response:?}");
    assert!(response.result.is_some(), "{response:?}");
}

const ACTORS: [Option<&str>; 5] = [
    None,
    Some(""),
    Some("  "),
    Some("local"),
    Some("agent:peer"),
];

#[tokio::test]
#[serial]
async fn strict_config_refuses_frames_without_an_attributed_actor() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    std::env::remove_var(STRICT_ENV);
    let responses = inbox_responses(comm_server(true), &ACTORS).await;
    for (actor, response) in ACTORS[..4].iter().zip(&responses[..4]) {
        assert_refused(response, *actor);
    }
    assert_served(&responses[4], ACTORS[4]);
}

#[tokio::test]
#[serial]
async fn strict_env_refuses_frames_without_an_attributed_actor() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    std::env::set_var(STRICT_ENV, "1");
    let responses = inbox_responses(comm_server(false), &ACTORS).await;
    std::env::remove_var(STRICT_ENV);
    for (actor, response) in ACTORS[..4].iter().zip(&responses[..4]) {
        assert_refused(response, *actor);
    }
    assert_served(&responses[4], ACTORS[4]);
}

#[tokio::test]
#[serial]
async fn without_strict_mode_every_frame_is_served() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    std::env::remove_var(STRICT_ENV);
    let responses = inbox_responses(comm_server(false), &ACTORS).await;
    for (actor, response) in ACTORS.iter().zip(&responses) {
        assert_served(response, *actor);
    }
}

#[tokio::test]
#[serial]
async fn strict_daemon_without_comm_serves_unattributed_frames() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    std::env::remove_var(STRICT_ENV);
    let mut config = KhiveRuntime::memory()
        .expect("memory runtime")
        .config()
        .clone();
    config.default_namespace = Namespace::parse("test").unwrap();
    config.packs = vec!["kg".to_string()];
    config.actor_id = Some("agent:owner".to_string());
    config.require_attributed_actor = true;
    let server = crate::server::KhiveMcpServer::new(KhiveRuntime::new(config).expect("runtime"))
        .expect("server builds with kg");

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    let mut frame = inbox_frame(server.config_id(), None);
    frame.ops = "stats()".to_string();
    let handle = tokio::spawn(async move {
        let _ = run_daemon(server).await;
    });
    drop(connect_when_ready(&sock).await);
    let response = exchange(&sock, &frame).await;
    handle.abort();
    let _ = handle.await;
    clear_daemon_env();
    assert_served(&response, None);
}
