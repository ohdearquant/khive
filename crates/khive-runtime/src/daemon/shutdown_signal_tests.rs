use super::*;
use std::io::Read as _;
use std::os::unix::process::ExitStatusExt as _;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct SignalChild(Child);

impl Drop for SignalChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn wait_for_child_marker(child: &mut SignalChild, path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            child.0.try_wait().expect("poll child").is_none(),
            "child exited before marker {}",
            path.display()
        );
        assert!(Instant::now() < deadline, "missing marker {}", path.display());
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn signal_child(child: &SignalChild, signal: i32) {
    let pid = i32::try_from(child.0.id()).expect("child PID fits pid_t");
    // SAFETY: the still-owned child PID and signal are valid; no memory is accessed.
    assert_eq!(unsafe { libc::kill(pid, signal) }, 0, "signal daemon child");
}

#[test]
fn second_signal_terminates_blocked_drain_and_cleanup() {
    for mode in ["drain", "cleanup"] {
        for first in [libc::SIGTERM, libc::SIGINT] {
            for second in [libc::SIGTERM, libc::SIGINT] {
                let dir = tempfile::Builder::new()
                    .prefix("kh-signal-")
                    .tempdir_in("/tmp")
                    .expect("isolated short socket directory");
                let sock = dir.path().join("s");
                let pid_path = dir.path().join("p");
                let lock_path = dir.path().join("l");
                let marker = dir.path().join("stopping");
                let mut child = SignalChild(
                    Command::new(std::env::current_exe().expect("test executable"))
                        .args([
                            "--exact",
                            "daemon::tests::shutdown_signals::shutdown_signal_child",
                            "--ignored",
                            "--nocapture",
                            "--test-threads=1",
                        ])
                        .env_clear()
                        .envs(std::env::vars_os().filter(|(key, _)| {
                            !key.to_string_lossy().starts_with("KHIVE_")
                        }))
                        .env("KHIVE_SOCKET", &sock)
                        .env("KHIVE_PID", &pid_path)
                        .env("KHIVE_LOCK", &lock_path)
                        .env("KHIVE_SIGNAL_TEST_MODE", mode)
                        .env("KHIVE_SIGNAL_TEST_MARKER", &marker)
                        .env(
                            "KHIVE_DRAIN_TIMEOUT_SECS",
                            if mode == "drain" { "600" } else { "0" },
                        )
                        .current_dir(dir.path())
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::inherit())
                        .spawn()
                        .expect("spawn isolated daemon child"),
                );
                wait_for_child_marker(&mut child, &sock);

                let mut stream = std::os::unix::net::UnixStream::connect(&sock)
                    .expect("connect readiness client");
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("bound readiness read");
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .expect("bound readiness write");
                let request = serde_json::to_vec(&base_request_frame("signal-test"))
                    .expect("encode readiness request");
                stream
                    .write_all(&(request.len() as u32).to_be_bytes())
                    .expect("write readiness length");
                stream.write_all(&request).expect("write readiness body");
                let mut length = [0; 4];
                stream.read_exact(&mut length).expect("read readiness length");
                let length = u32::from_be_bytes(length) as usize;
                assert!(length <= MAX_FRAME_BYTES, "bounded readiness response");
                let mut body = vec![0; length];
                stream.read_exact(&mut body).expect("read readiness body");
                let response: DaemonResponseFrame =
                    serde_json::from_slice(&body).expect("decode readiness response");
                assert!(response.ok, "readiness failed: {response:?}");
                drop(stream);

                let lock = (mode == "cleanup").then(|| {
                    let lock = open_lock_file(&lock_path).expect("open recovery lock");
                    // SAFETY: this owned descriptor stays open until the child is reaped.
                    assert_eq!(
                        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                        0,
                        "startup must have released the recovery lock"
                    );
                    lock
                });
                signal_child(&child, first);
                wait_for_child_marker(&mut child, &marker);

                // The first signal must preserve graceful shutdown while either
                // tracked work or the parent-held recovery flock prevents completion.
                let guard = Instant::now() + Duration::from_millis(250);
                while Instant::now() < guard {
                    assert!(
                        child.0.try_wait().expect("poll first signal").is_none(),
                        "first signal exited instead of starting shutdown: {mode}, {first}"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                signal_child(&child, second);
                let deadline = Instant::now() + Duration::from_secs(2);
                let status = loop {
                    if let Some(status) = child.0.try_wait().expect("poll second signal") {
                        break status;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "second signal failed to terminate blocked {mode}: {first} then {second}"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                };
                assert_eq!(
                    status.signal(),
                    Some(second),
                    "native second-signal status: {mode}: {status}"
                );
                assert!(sock.exists(), "second signal must abandon socket cleanup");
                assert_eq!(
                    std::fs::read_to_string(&pid_path).expect("abandoned PID file"),
                    child.0.id().to_string()
                );
                drop(lock);
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "subprocess helper, invoked by second_signal_terminates_blocked_drain_and_cleanup"]
async fn shutdown_signal_child() {
    let mode = std::env::var("KHIVE_SIGNAL_TEST_MODE").expect("isolated child mode");
    assert!(matches!(mode.as_str(), "drain" | "cleanup"));
    let marker = PathBuf::from(
        std::env::var_os("KHIVE_SIGNAL_TEST_MARKER").expect("isolated child marker"),
    );
    let _sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install child SIGTERM handler");
    let _sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .expect("install child SIGINT handler");
    if mode == "cleanup" {
        // The sole async executor blocks in flock, so observe cancellation from
        // an OS thread without adding work to the graceful-drain counter.
        let token = daemon_shutdown_token();
        let marker = marker.clone();
        std::thread::spawn(move || {
            while !token.is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
            std::fs::write(marker, b"cleanup").expect("publish shutdown marker");
        });
    }
    let dispatcher = MockDispatch {
        namespace: "local".to_string(),
        config_id: "signal-test".to_string(),
        dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        pool: None,
        dispatch_err: None,
    };
    let boot_guard = Some(acquire_daemon_boot_guard().expect("boot guard"));
    run_daemon_with_boot_guard_and_start(dispatcher, boot_guard, move |_| {
        if mode == "drain" {
            track_named_background_task("signal-test-held-drain", async move {
                daemon_shutdown_token().cancelled().await;
                std::fs::write(marker, b"drain").expect("publish shutdown marker");
                std::future::pending::<()>().await;
            });
        }
    })
    .await
    .expect("daemon shutdown");
    panic!("blocked daemon returned instead of awaiting the second signal");
}
