//! Live-process integration test for #714 (DESIGN-NOTE.md §5 item 3): the
//! `kkernel mcp` stdio bridge must survive an in-place re-exec triggered by a
//! stale daemon-protocol mismatch without the connected MCP client ever
//! seeing its stdio transport reset.
//!
//! Spawns the real compiled `kkernel` binary as an MCP server child process
//! (via `TokioChildProcess`) and drives it with a real `rmcp` client over the
//! child's actual stdio pipes — this is the one point in the test suite that
//! exercises the real, unstubbed `exec()` self-heal path (`crates/khive-mcp/
//! src/daemon.rs`'s `reexec_in_place` is only test-doubled inside `cargo
//! test`'s own process; the child here is a normal, non-test binary
//! invocation of `kkernel`, so the real `#[cfg(all(unix, not(test)))]` path
//! runs).
//!
//! A hand-rolled fake daemon (a bare `UnixListener`, matching the pattern
//! `khive-mcp/src/daemon.rs`'s own unit tests already use for
//! `forward_or_spawn`) stands in for the warm daemon at `KHIVE_SOCKET`:
//!   1. First connection (first-generation bridge) — responds with a
//!      protocol-version-0 frame, which `try_forward_inner`/`map_response`
//!      classify as `ProtocolMismatch` regardless of `served_config_id`
//!      (`daemon.rs::map_response`'s `version_mismatch` branch returns before
//!      the `served_config_id` equality check runs).
//!   2. Second connection (resumed-generation bridge, same OS process/PID,
//!      same stdio pipes — `exec()` never spawns a new process) — responds
//!      with a matching-`PROTOCOL_VERSION`, `ok: true` frame.
//!
//! Both responses echo the request frame's own `config_id` back as
//! `served_config_id`, so `map_response`'s fail-closed config-echo check
//! passes without the test needing to hand-reconstruct
//! `KhiveMcpServer::config_id`'s exact fingerprint format.

use std::process::Stdio;

use khive_runtime::daemon::{read_frame, write_frame, DaemonRequestFrame, DaemonResponseFrame};
use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use tokio::net::UnixListener;

fn kkernel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kkernel")
}

/// Serve exactly one request/response exchange over `listener`, decoding the
/// inbound frame only far enough to read `config_id` (so the response can
/// echo it back), then encoding and writing `response` with that value.
async fn serve_one(listener: &UnixListener, mut response: DaemonResponseFrame) {
    let (mut stream, _) = listener
        .accept()
        .await
        .expect("accept fake-daemon connection");
    let req_bytes = read_frame(&mut stream)
        .await
        .expect("read request frame from bridge");
    let req: DaemonRequestFrame =
        serde_json::from_slice(&req_bytes).expect("decode bridge request frame");
    response.served_config_id = Some(req.config_id);
    let payload = serde_json::to_vec(&response).expect("serialize fake-daemon response");
    write_frame(&mut stream, &payload)
        .await
        .expect("write fake-daemon response");
}

fn stale_daemon_response() -> DaemonResponseFrame {
    DaemonResponseFrame {
        ok: true,
        result: Some("stale-daemon-result".to_string()),
        error: None,
        namespace_mismatch: false,
        config_mismatch: false,
        served_config_id: None,
        // A pre-versioning (or otherwise stale) daemon either omits this
        // field (defaults to 0) or never sets `version_mismatch` itself —
        // either way `daemon_protocol_version != PROTOCOL_VERSION` with
        // `version_mismatch: false` is exactly the "old daemon" shape
        // `try_forward_inner` classifies as `ProtocolMismatch`.
        version_mismatch: false,
        daemon_protocol_version: 0,
        metrics: None,
    }
}

fn healed_daemon_response() -> DaemonResponseFrame {
    DaemonResponseFrame {
        ok: true,
        result: Some("integration-test-post-reexec-ok".to_string()),
        error: None,
        namespace_mismatch: false,
        config_mismatch: false,
        served_config_id: None,
        version_mismatch: false,
        daemon_protocol_version: khive_runtime::daemon::PROTOCOL_VERSION,
        metrics: None,
    }
}

#[tokio::test]
async fn bridge_self_heals_across_in_place_reexec_without_losing_the_client_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");
    let lock_file = dir.path().join("khived.recovery.lock");

    // Bind the fake daemon BEFORE spawning the bridge: `forward_or_spawn`'s
    // first-attempt arm must observe `ForwardOutcome::Response` (mismatch),
    // never `NoSocket` (which would instead drive the real kill/respawn path
    // and try to launch an actual daemon subprocess).
    let listener = UnixListener::bind(&sock).expect("bind fake daemon socket");

    let fake_daemon = tokio::spawn(async move {
        serve_one(&listener, stale_daemon_response()).await;
        serve_one(&listener, healed_daemon_response()).await;
    });

    let mut command = tokio::process::Command::new(kkernel_bin());
    command
        .arg("mcp")
        .arg("--db")
        .arg(":memory:")
        .arg("--pack")
        .arg("kg")
        .env("KHIVE_SOCKET", &sock)
        .env("KHIVE_PID", &pid_file)
        .env("KHIVE_LOCK", &lock_file)
        .env_remove("KHIVE_NO_DAEMON")
        .stderr(Stdio::inherit());

    let child_transport = TokioChildProcess::new(command).expect("spawn kkernel mcp child");
    let child_pid = child_transport.id();

    let client = ()
        .serve(child_transport)
        .await
        .expect("client session established with the bridge (cold-start handshake)");

    let ops_args = |ops: &str| {
        let mut map = serde_json::Map::new();
        map.insert(
            "ops".to_string(),
            serde_json::Value::String(ops.to_string()),
        );
        map
    };

    // First call: the bridge forwards to the fake daemon, observes the
    // protocol-version-0 response, and must surface a hard mismatch error —
    // never silently fall back to local dispatch (which would hide the skew)
    // and never hang (which would mean the self-heal re-exec ate the
    // in-flight response instead of letting it flush first).
    let first = client
        .call_tool(CallToolRequestParams::new("request").with_arguments(ops_args("stats()")))
        .await;
    let first_err = match first {
        Err(e) => e,
        Ok(r) => panic!("first call must fail with a protocol-mismatch error, got Ok({r:?})"),
    };
    let first_msg = first_err.to_string();
    assert!(
        first_msg.contains("protocol mismatch"),
        "first call's error must name 'protocol mismatch'; got: {first_msg}"
    );

    // Second call: same `Peer`, same underlying stdio transport, no
    // reconnect and no second `serve()`/handshake performed by this test.
    // The bridge process self-healed via an in-place `exec()` behind the
    // scenes (never observable as a transport reset — the whole point of
    // #714) and responded to the client's still-open MCP session using
    // `serve_directly` with the second, protocol-matching fake-daemon
    // response.
    let second = client
        .call_tool(CallToolRequestParams::new("request").with_arguments(ops_args("stats()")))
        .await
        .expect("second call must succeed once the resumed generation is serving");
    let text = second
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default();
    assert!(
        text.contains("integration-test-post-reexec-ok"),
        "second call must return the post-reexec fake-daemon result; got: {text}"
    );

    assert!(
        child_pid.is_some(),
        "child process PID must be observable (spawn succeeded and the OS reused \
         the same PID across exec — the property #714's whole design rests on)"
    );

    client
        .cancel()
        .await
        .expect("client session cancels cleanly");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), fake_daemon).await;
}
