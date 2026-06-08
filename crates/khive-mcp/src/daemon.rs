//! khived daemon client — forwarding + auto-spawn.
//!
//! The daemon server lives in `khive-runtime::daemon`. This module provides the
//! client side: [`forward_or_spawn`] connects to the daemon, auto-spawns it on
//! first use, and maps responses to MCP error types. Every failure path falls
//! back to `None` so the caller can dispatch locally.
//!
//! Also provides the [`khive_runtime::daemon::DaemonDispatch`] impl for [`crate::server::KhiveMcpServer`].

use std::process::Stdio;

use async_trait::async_trait;
use khive_runtime::daemon::{
    self, env_truthy, read_frame, socket_path, write_frame, DaemonRequestFrame, DaemonResponseFrame,
};
use rmcp::ErrorData as McpError;
use tokio::net::UnixStream;

use crate::tools::request::RequestParams;

// ── DaemonDispatch impl ───────────────────────────────────────────────────────

#[async_trait]
impl daemon::DaemonDispatch for crate::server::KhiveMcpServer {
    async fn dispatch(
        &self,
        ops: String,
        presentation: Option<String>,
        presentation_per_op: Option<Vec<Option<String>>>,
    ) -> Result<String, String> {
        let params = RequestParams {
            ops,
            presentation,
            presentation_per_op,
        };
        self.dispatch_request_local(params)
            .await
            .map_err(|e| e.message.to_string())
    }

    async fn warm_all(&self) {
        crate::server::KhiveMcpServer::warm_all(self).await;
    }

    fn namespace(&self) -> &str {
        self.default_namespace()
    }

    fn config_id(&self) -> &str {
        crate::server::KhiveMcpServer::config_id(self)
    }
}

// ── client ────────────────────────────────────────────────────────────────────

async fn try_forward(frame: &DaemonRequestFrame) -> Option<DaemonResponseFrame> {
    let mut stream = UnixStream::connect(socket_path()).await.ok()?;
    let payload = serde_json::to_vec(frame).ok()?;
    write_frame(&mut stream, &payload).await.ok()?;
    let resp = read_frame(&mut stream).await.ok()?;
    serde_json::from_slice::<DaemonResponseFrame>(&resp).ok()
}

fn map_response(
    resp: DaemonResponseFrame,
    expected_config_id: &str,
) -> Option<Result<String, McpError>> {
    if resp.namespace_mismatch || resp.config_mismatch {
        return None;
    }
    // Fail closed: only trust a result the daemon positively confirms it served
    // under our exact config. A legacy daemon omits `served_config_id` (→ None)
    // and a config-drifted daemon echoes a different id — both fall back local.
    if resp.served_config_id.as_deref() != Some(expected_config_id) {
        return None;
    }
    if resp.ok {
        Some(Ok(resp.result.unwrap_or_default()))
    } else {
        let msg = resp
            .error
            .unwrap_or_else(|| "daemon returned an error without a message".to_string());
        Some(Err(McpError::internal_error(msg, None)))
    }
}

fn spawn_daemon() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    // The binary is `kkernel`; the MCP server (and its daemon mode) live under
    // the `mcp` subcommand.
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("mcp")
        .arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()?;
    Ok(())
}

/// Forward a request to the daemon, auto-spawning it if absent.
///
/// Returns `None` on any failure → caller falls back to local dispatch.
pub async fn forward_or_spawn(frame: &DaemonRequestFrame) -> Option<Result<String, McpError>> {
    if env_truthy("KHIVE_NO_DAEMON") {
        return None;
    }

    if let Some(resp) = try_forward(frame).await {
        return map_response(resp, &frame.config_id);
    }

    if spawn_daemon().is_err() {
        return None;
    }

    let sock = socket_path();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if UnixStream::connect(&sock).await.is_ok() {
            return try_forward(frame)
                .await
                .and_then(|resp| map_response(resp, &frame.config_id));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    None
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::daemon::run_daemon;
    use serial_test::serial;

    use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};

    fn make_test_server() -> crate::server::KhiveMcpServer {
        let config = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::parse("test").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "gtd".to_string()],
            ..RuntimeConfig::default()
        };
        let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
        crate::server::KhiveMcpServer::new(runtime).expect("server builds with kg+gtd")
    }

    fn clear_daemon_env() {
        std::env::remove_var("KHIVE_SOCKET");
        std::env::remove_var("KHIVE_PID");
        std::env::remove_var("KHIVE_NO_DAEMON");
    }

    async fn connect_when_ready(sock: &std::path::Path) -> UnixStream {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(s) = UnixStream::connect(sock).await {
                return s;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "daemon never bound {sock:?} within 5s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    async fn exchange(sock: &std::path::Path, frame: &DaemonRequestFrame) -> DaemonResponseFrame {
        let mut stream = UnixStream::connect(sock)
            .await
            .expect("connect to daemon socket");
        let payload = serde_json::to_vec(frame).expect("serialize request frame");
        write_frame(&mut stream, &payload)
            .await
            .expect("write request frame");
        let resp = read_frame(&mut stream).await.expect("read response frame");
        serde_json::from_slice(&resp).expect("decode response frame")
    }

    // ── map_response (pure, MCP-specific) ─────────────────────────────────────

    const CFG: &str = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

    #[test]
    fn map_response_namespace_mismatch_yields_none() {
        let resp = DaemonResponseFrame {
            ok: false,
            result: None,
            error: None,
            namespace_mismatch: true,
            config_mismatch: false,
            served_config_id: Some(CFG.to_string()),
        };
        assert!(map_response(resp, CFG).is_none());
    }

    #[test]
    fn map_response_config_mismatch_yields_none() {
        let resp = DaemonResponseFrame {
            ok: false,
            result: None,
            error: None,
            namespace_mismatch: false,
            config_mismatch: true,
            served_config_id: Some(CFG.to_string()),
        };
        assert!(map_response(resp, CFG).is_none());
    }

    #[test]
    fn map_response_legacy_daemon_missing_echo_yields_none() {
        // A pre-config_id daemon omits served_config_id (→ None). Even on an
        // ok=true result the client MUST fall back to local dispatch.
        let resp = DaemonResponseFrame {
            ok: true,
            result: Some("served-by-broad-registry".to_string()),
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: None,
        };
        assert!(map_response(resp, CFG).is_none());
    }

    #[test]
    fn map_response_echo_drift_yields_none() {
        // A daemon serving under a different config (echo != expected) is not
        // trusted, even without an explicit config_mismatch flag.
        let resp = DaemonResponseFrame {
            ok: true,
            result: Some("served-by-other-config".to_string()),
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(
                "packs=[kg,gtd];db=/x;embed=none;extra=[];backend=main".to_string(),
            ),
        };
        assert!(map_response(resp, CFG).is_none());
    }

    #[test]
    fn map_response_ok_with_result_yields_some_ok() {
        let resp = DaemonResponseFrame {
            ok: true,
            result: Some("the-result".to_string()),
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(CFG.to_string()),
        };
        match map_response(resp, CFG) {
            Some(Ok(s)) => assert_eq!(s, "the-result"),
            other => panic!("expected Some(Ok(\"the-result\")), got {other:?}"),
        }
    }

    #[test]
    fn map_response_ok_with_no_result_yields_some_ok_empty() {
        let resp = DaemonResponseFrame {
            ok: true,
            result: None,
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(CFG.to_string()),
        };
        match map_response(resp, CFG) {
            Some(Ok(s)) => assert_eq!(s, ""),
            other => panic!("expected Some(Ok(\"\")), got {other:?}"),
        }
    }

    #[test]
    fn map_response_not_ok_yields_some_err_preserving_message() {
        let resp = DaemonResponseFrame {
            ok: false,
            result: None,
            error: Some("boom: bad verb".to_string()),
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(CFG.to_string()),
        };
        match map_response(resp, CFG) {
            Some(Err(McpError { message, .. })) => {
                assert!(message.contains("boom: bad verb"));
            }
            other => panic!("expected Some(Err(..)), got {other:?}"),
        }
    }

    #[test]
    fn map_response_not_ok_without_message_yields_default_err() {
        let resp = DaemonResponseFrame {
            ok: false,
            result: None,
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(CFG.to_string()),
        };
        match map_response(resp, CFG) {
            Some(Err(McpError { message, .. })) => {
                assert!(!message.is_empty());
            }
            other => panic!("expected Some(Err(..)), got {other:?}"),
        }
    }

    // ── forward_or_spawn fallback (env-mutating → serial) ─────────────────────

    #[tokio::test]
    #[serial]
    async fn forward_or_spawn_returns_none_when_no_daemon_set() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
        std::env::set_var("KHIVE_NO_DAEMON", "1");

        let frame = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            config_id: "test".to_string(),
        };
        let out = forward_or_spawn(&frame).await;
        assert!(out.is_none());
        assert!(!sock.exists());

        clear_daemon_env();
    }

    // ── daemon socket round-trip (env-mutating → serial) ─────────────────────

    #[tokio::test]
    #[serial]
    async fn daemon_round_trip_dispatches_and_enforces_namespace() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid = dir.path().join("khived.pid");
        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let reference = make_test_server();
        let config_id = reference.config_id().to_string();
        let daemon_server = reference.clone();

        let handle = tokio::spawn(async move {
            let _ = run_daemon(daemon_server).await;
        });

        let _ready = connect_when_ready(&sock).await;
        drop(_ready);

        // (a) valid same-namespace, same-config op
        let req = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            namespace: "test".to_string(),
            config_id: config_id.clone(),
        };
        let resp = exchange(&sock, &req).await;
        assert!(resp.ok, "valid op must succeed; error={:?}", resp.error);
        assert!(!resp.namespace_mismatch);
        assert!(!resp.config_mismatch);
        assert_eq!(
            resp.served_config_id.as_deref(),
            Some(config_id.as_str()),
            "daemon must echo the config it served under"
        );

        let reference_result = reference
            .dispatch_request_local(RequestParams {
                ops: "stats()".to_string(),
                presentation: Some("verbose".to_string()),
                presentation_per_op: None,
            })
            .await
            .expect("local dispatch of stats() must succeed");
        assert_eq!(resp.result.as_deref(), Some(reference_result.as_str()));
        assert!(reference_result.contains("\"entities\""));

        // (b) different namespace → namespace_mismatch (config matches)
        let other = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "other".to_string(),
            config_id: config_id.clone(),
        };
        let resp_other = exchange(&sock, &other).await;
        assert!(resp_other.namespace_mismatch);
        assert!(!resp_other.ok);

        // (c) same namespace but different config (e.g. a `--pack kg` client
        // hitting the broader daemon) → config_mismatch, no dispatch.
        let mismatched_config = DaemonRequestFrame {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            namespace: "test".to_string(),
            config_id: "packs=[kg];db=:memory:;embed=none;extra=[];backend=main".to_string(),
        };
        let resp_cfg = exchange(&sock, &mismatched_config).await;
        assert!(
            resp_cfg.config_mismatch,
            "differing config must be rejected"
        );
        assert!(!resp_cfg.namespace_mismatch);
        assert!(!resp_cfg.ok);

        handle.abort();
        let _ = handle.await;
        clear_daemon_env();
    }
}
