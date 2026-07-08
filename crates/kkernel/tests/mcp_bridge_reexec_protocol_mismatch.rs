//! Live-process integration test for #714 (self-heal test plan item 3): the
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
//!
//! Critically, the second `call_tool` is withheld until a resumed-generation
//! log line is observed on the child's stderr. Without this gate the test is
//! vacuous: even a bridge that never re-execs at all would still serve the
//! second request correctly (the original, un-exec'd process just opens a
//! *second* connection to the fake daemon socket and relays whatever it
//! gets back), so a passing test proves nothing about the exec path unless
//! it first proves the exec actually happened. `crates/khive-mcp/src/serve.rs`
//! logs `tracing::warn!(generation, "bridge self-heal: this process is a
//! resumed generation...")` at the very top of `run()`, before any request
//! handling — a line only a process started with the `--resumed-generation`
//! marker can ever emit, and the only way that marker exists is a completed
//! `exec()` (this test never spawns a second process itself).

use std::process::Stdio;
use std::time::Duration;

use khive_runtime::daemon::{read_frame, write_frame, DaemonRequestFrame, DaemonResponseFrame};
use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixListener;

const RESUMED_GENERATION_LOG_NEEDLE: &str = "resumed generation of an in-place re-exec";
const RESUMED_GENERATION_WAIT: Duration = Duration::from_secs(10);

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

/// Block until the child's stderr emits the resumed-generation self-heal log
/// line, or panic after [`RESUMED_GENERATION_WAIT`]. This is the test's
/// actual exec-path gate — see the module doc for why waiting on this
/// specific signal (rather than any fixed delay) is what makes the test
/// non-vacuous.
async fn wait_for_resumed_generation_log(stderr: tokio::process::ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    let deadline = tokio::time::Instant::now() + RESUMED_GENERATION_WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!(
                "timed out after {RESUMED_GENERATION_WAIT:?} waiting for the \
                 resumed-generation log line on the child's stderr — the bridge \
                 self-heal re-exec never fired; #714's whole design rests on this \
                 happening after a protocol mismatch"
            );
        }
        match tokio::time::timeout(remaining, lines.next_line()).await {
            Ok(Ok(Some(line))) => {
                if line.contains(RESUMED_GENERATION_LOG_NEEDLE) {
                    return;
                }
            }
            Ok(Ok(None)) => panic!(
                "child stderr closed before the resumed-generation log line ever \
                 appeared — the bridge exited instead of self-healing"
            ),
            Ok(Err(e)) => panic!("error reading child stderr while waiting for self-heal: {e}"),
            Err(_) => panic!(
                "timed out after {RESUMED_GENERATION_WAIT:?} waiting for the \
                 resumed-generation log line on the child's stderr"
            ),
        }
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
        .env_remove("KHIVE_NO_DAEMON");

    let (child_transport, stderr) = TokioChildProcess::builder(command)
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kkernel mcp child");
    let stderr = stderr.expect("stderr must be captured (requested Stdio::piped())");
    let pid_before = child_transport.id();

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

    // Do NOT send the second request until the resumed generation has
    // actually announced itself. This is the test's real exec-path gate —
    // see the module doc for why skipping it makes the test pass regardless
    // of whether exec ever fired.
    wait_for_resumed_generation_log(stderr).await;

    // The PID captured before the mismatch must still be alive after the
    // resumed-generation signal: `exec()` replaces the process image but
    // never the PID, so this is an OS-level (not just Rust-handle-level)
    // check that the SAME process — not a newly spawned one — is what
    // logged the resumed-generation line and is about to serve the second
    // request. `TokioChildProcess`'s handle is unavailable here regardless
    // (moved into `client` by `.serve()` above), which is itself part of
    // the property under test: #714 is designed so nothing about serving
    // the resumed generation requires a second process handle at all.
    let pid_before = pid_before.expect("child PID must be observable (spawn succeeded)");
    assert!(
        pid_is_alive(pid_before),
        "PID {pid_before} (captured before the mismatch) must still be alive after \
         the resumed-generation signal — exec() preserves the PID; if it were gone, \
         a new process would have had to be spawned instead of an in-place re-exec"
    );

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

    client
        .cancel()
        .await
        .expect("client session cancels cleanly");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), fake_daemon).await;
}

/// OS-level (not just Rust-handle-level) liveness check via `ps`, used to
/// confirm the PID observed before the mismatch is still running after the
/// resumed-generation signal — independent evidence that `exec()` preserved
/// the process rather than a new one having been spawned.
fn pid_is_alive(pid: u32) -> bool {
    std::process::Command::new("ps")
        .arg("-p")
        .arg(pid.to_string())
        .arg("-o")
        .arg("pid=")
        .output()
        .map(|out| out.status.success() && !out.stdout.is_empty())
        .unwrap_or(false)
}
