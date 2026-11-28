use super::{
    forward_or_spawn, forward_or_spawn_with_config_and_packs, validate_primary_socket_path,
};
use crate::daemon::test_harness::{clear_daemon_env, RecoveryTestGuard};
use crate::daemon::{
    fallback_strict_violations, fallback_total, reset_counters, reset_fallback_counters,
    KILL_COUNT, SIGTERM_COUNT, SPAWN_COUNT, STRICT_FALLBACK_MARKER,
};
use khive_runtime::daemon::{
    read_frame, socket_path, write_frame, DaemonRequestFrame, DaemonResponseFrame, PROTOCOL_VERSION,
};
use rmcp::ErrorData as McpError;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;

fn capacity() -> usize {
    // SAFETY: only the size of the character array is inspected.
    let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_path.len()
}

fn assert_path_refusal(error: &McpError, path: &Path) {
    let bytes = path.as_os_str().as_bytes().len();
    assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    let data = error.data.as_ref().expect("pathname refusal data");
    assert_eq!(data["reason"], "daemon_socket_path_too_long");
    assert_eq!(data["domain_disposition"], "not_committed");
    assert_eq!(data["path"], format!("{path:?}"));
    assert_eq!(data["path_bytes"], bytes);
    assert_eq!(data["required_bytes"], bytes + 1);
    assert_eq!(data["sun_path_capacity"], capacity());
    assert!(data.get(STRICT_FALLBACK_MARKER).is_none());
    assert!(error.message.contains(&format!("{path:?}")));
    assert!(error.message.contains(&format!("{bytes} path bytes")));
    assert!(error.message.contains("including NUL"));
    assert!(error.message.contains("paired shorter private absolute"));
    assert!(error.message.contains("KHIVE_SOCKET and KHIVE_PID"));
    assert!(error
        .message
        .contains("consistently for the daemon and its clients"));
}

#[test]
fn primary_socket_path_counts_raw_bytes_and_terminating_nul() {
    let limit = capacity();
    let maximum = PathBuf::from("x".repeat(limit - 1));
    validate_primary_socket_path(&maximum).expect("terminating NUL exactly fits");
    let oversized = PathBuf::from("x".repeat(limit));
    assert_path_refusal(
        &validate_primary_socket_path(&oversized).unwrap_err(),
        &oversized,
    );

    let unicode = PathBuf::from("é".repeat(limit / 2 + 1));
    assert!(unicode.to_str().unwrap().chars().count() < limit);
    assert_path_refusal(
        &validate_primary_socket_path(&unicode).unwrap_err(),
        &unicode,
    );

    // The resolver intentionally ignores non-Unicode environment overrides;
    // these cases test the pure byte checker, not a different resolver policy.
    let non_utf8 = PathBuf::from(OsString::from_vec(vec![0xff; limit - 1]));
    assert!(non_utf8.to_str().is_none());
    validate_primary_socket_path(&non_utf8).expect("raw bytes fit despite lossy expansion");
    let non_utf8_long = PathBuf::from(OsString::from_vec(vec![0xff; limit]));
    assert_path_refusal(
        &validate_primary_socket_path(&non_utf8_long).unwrap_err(),
        &non_utf8_long,
    );
}

fn frame() -> DaemonRequestFrame {
    DaemonRequestFrame {
        ops: "stats()".into(),
        namespace: "local".into(),
        config_id: "primary-path-fixture".into(),
        protocol_version: PROTOCOL_VERSION,
        ..Default::default()
    }
}

async fn forward(configured: bool, frame: &DaemonRequestFrame) -> Option<Result<String, McpError>> {
    if configured {
        forward_or_spawn_with_config_and_packs(frame, None, None, None).await
    } else {
        forward_or_spawn(frame).await
    }
}

fn no_lifecycle_or_fallback() {
    assert_eq!(SPAWN_COUNT.load(SeqCst), 0, "actual launcher entries");
    assert_eq!(KILL_COUNT.load(SeqCst), 0);
    assert_eq!(SIGTERM_COUNT.load(SeqCst), 0);
    assert_eq!(fallback_total(), 0);
    assert_eq!(fallback_strict_violations(), 0);
}

fn files(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    fn visit(root: &Path, directory: &Path, out: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_owned();
            if entry.file_type().unwrap().is_dir() {
                out.push((relative, None));
                visit(root, &path, out);
            } else {
                assert!(entry.file_type().unwrap().is_file());
                out.push((relative, Some(std::fs::read(path).unwrap())));
            }
        }
    }
    let mut out = Vec::new();
    visit(root, root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn directory() -> tempfile::TempDir {
    // /tmp keeps socket paths short; where /tmp is closed, as in a sandboxed
    // run, the process's own temporary directory takes its place.
    let directory = tempfile::Builder::new()
        .prefix("primary-path-")
        .tempdir_in("/tmp")
        .or_else(|_| tempfile::Builder::new().prefix("primary-path-").tempdir())
        .unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

#[tokio::test]
#[serial_test::serial]
async fn public_forwarders_refuse_oversized_primary_socket_before_lifecycle() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    clear_daemon_env();
    reset_counters();
    reset_fallback_counters();
    let _cleanup = RecoveryTestGuard::new();
    let directory = directory();
    let root = directory.path().canonicalize().unwrap();
    let oversized = root.join("x".repeat(capacity()));
    std::fs::write(root.join("unchanged"), b"private fixture\n").unwrap();
    let before = files(&root);
    let frame = frame();

    for default_home in [false, true] {
        if default_home {
            std::env::remove_var("KHIVE_SOCKET");
            std::env::remove_var("KHIVE_PID");
            // No filesystem access is needed to resolve the default socket.
            std::env::set_var("HOME", &oversized);
        } else {
            std::env::set_var("KHIVE_SOCKET", &oversized);
            std::env::set_var("KHIVE_PID", root.join("daemon.pid"));
        }
        let resolved = socket_path();
        assert_eq!(
            resolved,
            if default_home {
                oversized.join(".khive/khived.sock")
            } else {
                oversized.clone()
            }
        );
        for strict in ["0", "1"] {
            std::env::set_var("KHIVE_DAEMON_STRICT", strict);
            for configured in [false, true] {
                let result =
                    tokio::time::timeout(Duration::from_secs(2), forward(configured, &frame))
                        .await
                        .expect("invalid pathname must settle before recovery");
                let error = result
                    .expect("invalid pathname must not fall back")
                    .unwrap_err();
                assert_path_refusal(&error, &resolved);
                no_lifecycle_or_fallback();
                assert_eq!(files(&root), before);
            }
            std::env::set_var("KHIVE_NO_DAEMON", "1");
            for configured in [false, true] {
                assert!(forward(configured, &frame).await.is_none());
            }
            std::env::remove_var("KHIVE_NO_DAEMON");
            no_lifecycle_or_fallback();
            assert_eq!(files(&root), before);
        }
    }
}

async fn forward_to_peer(path: &Path, configured: bool, frame: &DaemonRequestFrame) {
    std::env::set_var("KHIVE_SOCKET", path);
    let listener = tokio::net::UnixListener::bind(path).expect("valid socket pathname binds");
    let peer = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request: DaemonRequestFrame =
            serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
        assert_eq!(request.ops, frame.ops);
        let response = DaemonResponseFrame {
            ok: true,
            result: Some("primary-socket-peer".into()),
            error: None,
            error_detail: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(request.config_id),
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: request.request_id,
        };
        write_frame(&mut stream, &serde_json::to_vec(&response).unwrap())
            .await
            .unwrap();
    };
    let response = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::pin!(peer);
        let request = forward(configured, frame);
        tokio::pin!(request);
        // An incorrect pre-connect refusal must fail the result assertion,
        // rather than waiting for a peer that never receives a connection.
        tokio::select! {
            response = &mut request => response,
            () = &mut peer => request.await,
        }
    })
    .await
    .expect("valid socket exchange must settle");
    drop(listener);
    std::fs::remove_file(path).unwrap();
    assert_eq!(
        response
            .expect("must forward")
            .expect("valid pathname must be admitted"),
        "primary-socket-peer"
    );
    no_lifecycle_or_fallback();
}

#[tokio::test]
#[serial_test::serial]
async fn public_forwarders_accept_boundary_and_relative_primary_sockets() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    clear_daemon_env();
    reset_counters();
    reset_fallback_counters();
    let _cleanup = RecoveryTestGuard::new();
    let directory = directory();
    let root = directory.path().canonicalize().unwrap();
    let prefix = root.as_os_str().as_bytes().len() + 1;
    assert!(prefix < capacity() - 1);
    let maximum = root.join("s".repeat(capacity() - 1 - prefix));
    assert_eq!(maximum.as_os_str().as_bytes().len() + 1, capacity());
    std::env::set_var("KHIVE_PID", root.join("daemon.pid"));
    let before = files(&root);
    let frame = frame();
    for strict in ["0", "1"] {
        std::env::set_var("KHIVE_DAEMON_STRICT", strict);
        for configured in [false, true] {
            forward_to_peer(&maximum, configured, &frame).await;
            assert_eq!(files(&root), before);
        }
    }

    let long_cwd = root.join("d".repeat(capacity()));
    std::fs::create_dir(&long_cwd).unwrap();
    std::fs::set_permissions(&long_cwd, std::fs::Permissions::from_mode(0o700)).unwrap();
    struct RestoreCwd(PathBuf);
    impl Drop for RestoreCwd {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).unwrap();
        }
    }
    let _restore_cwd = RestoreCwd(std::env::current_dir().unwrap());
    std::env::set_current_dir(&long_cwd).unwrap();
    let relative = Path::new("daemon.sock");
    assert!(
        std::env::current_dir()
            .unwrap()
            .join(relative)
            .as_os_str()
            .as_bytes()
            .len()
            + 1
            > capacity()
    );
    std::env::set_var("KHIVE_PID", "daemon.pid");
    let before = files(&root);
    for strict in ["0", "1"] {
        std::env::set_var("KHIVE_DAEMON_STRICT", strict);
        for configured in [false, true] {
            forward_to_peer(relative, configured, &frame).await;
            assert_eq!(socket_path(), relative);
            assert_eq!(files(&root), before);
        }
    }
}
