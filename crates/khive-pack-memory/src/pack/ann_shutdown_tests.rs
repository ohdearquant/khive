use super::*;

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    os::unix::net::UnixDatagram,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::ann::interactive_build_test_support as gate;

const SETUP: Duration = Duration::from_secs(30);
const HELD: Duration = Duration::from_secs(5);
const SETTLE: Duration = Duration::from_secs(20);
const FAILSAFE: Duration = Duration::from_secs(60);
const WATCHDOG: Duration = Duration::from_secs(90);
const POLL: Duration = Duration::from_millis(20);

const ROOT: &str = "KHIVE_TEST_6339_ROOT";
const NONCE: &str = "KHIVE_TEST_6339_NONCE";
const MODE: &str = "KHIVE_TEST_6339_MODE";

enum ExitReceive {
    Frame,
    Poll,
    PeerClosed,
}

// A false result is only a poll timeout, not EOF. UnixDatagram zero-length
// frames still fail receive's nonce validation; only a distinct error can close.
fn classify_exit_receive(result: io::Result<bool>, complete: bool) -> io::Result<ExitReceive> {
    match result {
        Ok(true) => Ok(ExitReceive::Frame),
        Ok(false) => Ok(ExitReceive::Poll),
        Err(error)
            if complete
                && matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof
                ) =>
        {
            Ok(ExitReceive::PeerClosed)
        }
        Err(error) => Err(error),
    }
}

#[derive(Clone, Copy)]
struct InjectedPeerClose {
    before_child_ok: bool,
    kind: io::ErrorKind,
}

struct ChildRun {
    directory: Option<tempfile::TempDir>,
    child: Child,
    socket: UnixDatagram,
    child_socket: PathBuf,
    nonce: String,
    mode: &'static str,
    events: Vec<String>,
    connected: bool,
    release: Option<&'static str>,
    status: Option<ExitStatus>,
    cleanup_ok: bool,
    cleanup_started: bool,
    terminal_ipc_closed: bool,
    deadline: Instant,
}

impl ChildRun {
    fn spawn(mode: &'static str) -> Self {
        let directory = tempfile::Builder::new()
            .prefix("k6339-")
            .tempdir_in(std::env::temp_dir())
            .expect("owned child directory");
        let socket = UnixDatagram::bind(directory.path().join("p")).expect("parent Unix IPC");
        socket.set_read_timeout(Some(POLL)).expect("IPC poll bound");
        socket
            .set_write_timeout(Some(POLL))
            .expect("IPC write bound");
        let nonce = format!(
            "{:x}{:x}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        let child_name = format!(
            "{}::cold_miss_child",
            module_path!().split_once("::").expect("test module path").1
        );
        let stdout = File::create(directory.path().join("stdout")).expect("owned stdout");
        let stderr = File::create(directory.path().join("stderr")).expect("owned stderr");
        let readiness_ms = if mode == "open" {
            (SETUP + Duration::from_secs(5)).as_millis().to_string()
        } else {
            "1000".to_owned()
        };
        let deadline = Instant::now() + WATCHDOG;
        let child = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                &child_name,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROOT, directory.path())
            .env(NONCE, &nonce)
            .env(MODE, mode)
            // ann_ready_timeout_ms parses this existing key once per process, without a clamp.
            .env("KHIVE_MEMORY_ANN_READY_TIMEOUT_MS", readiness_ms)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("exact owned child");
        Self {
            child_socket: directory.path().join("c"),
            directory: Some(directory),
            child,
            socket,
            nonce,
            mode,
            events: Vec::new(),
            connected: false,
            release: None,
            status: None,
            cleanup_ok: true,
            cleanup_started: false,
            terminal_ipc_closed: false,
            deadline,
        }
    }

    fn has(&self, event: &str) -> bool {
        self.events.iter().any(|seen| seen == event)
    }

    fn counts(&self) -> Option<(usize, usize)> {
        let event = self
            .events
            .iter()
            .find_map(|event| event.strip_prefix("COUNTS "))?;
        let (captures, entries) = event.split_once(' ')?;
        Some((captures.parse().ok()?, entries.parse().ok()?))
    }

    fn receive(&mut self, deadline: Instant) -> io::Result<bool> {
        let remaining = deadline
            .min(self.deadline)
            .saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        self.socket.set_read_timeout(Some(POLL.min(remaining)))?;
        let mut bytes = [0; 1024];
        let (length, sender) = match self.socket.recv_from(&mut bytes) {
            Ok(frame) => frame,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return Ok(false)
            }
            Err(error) => return Err(error),
        };
        if !self.connected {
            self.socket.connect(&self.child_socket)?;
            self.connected = true;
        }
        if sender.as_pathname() != Some(self.child_socket.as_path()) || length == bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "child IPC sender/frame",
            ));
        }
        let message = std::str::from_utf8(&bytes[..length])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let event = message
            .strip_prefix(&self.nonce)
            .and_then(|message| message.strip_prefix(' '))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "child IPC nonce"))?;
        eprintln!("6339 {}: {event}", self.mode);
        self.events.push(event.to_owned());
        Ok(true)
    }

    fn accept_exit_receive(&mut self, result: io::Result<bool>) -> io::Result<bool> {
        let complete = self.has("CHILD_OK") && self.has("POST_DROP");
        match classify_exit_receive(result, complete)? {
            ExitReceive::Frame => Ok(true),
            ExitReceive::Poll => Ok(false),
            ExitReceive::PeerClosed => {
                self.terminal_ipc_closed = true;
                Ok(false)
            }
        }
    }

    fn receive_for_exit(&mut self, deadline: Instant) -> io::Result<bool> {
        if self.terminal_ipc_closed {
            let remaining = deadline
                .min(self.deadline)
                .saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                std::thread::sleep(POLL.min(remaining));
            }
            return Ok(false);
        }
        let received = self.receive(deadline);
        self.accept_exit_receive(received)
    }

    fn send_release(&mut self, cleanup: bool) -> io::Result<Instant> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "child watchdog reached",
            ));
        }
        self.socket.set_write_timeout(Some(POLL.min(remaining)))?;
        if !self.connected {
            self.socket.connect(&self.child_socket)?;
            self.connected = true;
        }
        let event = if cleanup {
            "CLEANUP_RELEASE"
        } else {
            "RELEASE"
        };
        let frame = format!("{} {event}", self.nonce);
        let sent = self.socket.send(frame.as_bytes())?;
        let delivered_at = Instant::now();
        if sent != frame.len() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "parent IPC frame"));
        }
        self.release = Some(if cleanup { "CLEANUP" } else { "NORMAL" });
        eprintln!("6339 {} parent: {event}", self.mode);
        Ok(delivered_at)
    }

    fn poll_status(&mut self) -> io::Result<bool> {
        if self.status.is_none() {
            if let Some(status) = self.child.try_wait()? {
                self.status = Some(status);
                eprintln!("6339 {} parent: REAPED {:?}", self.mode, self.status);
            }
        }
        Ok(self.status.is_some())
    }

    fn collect_until(&mut self, budget: Duration, ready: impl Fn(&Self) -> bool) -> bool {
        let deadline = (Instant::now() + budget).min(self.deadline);
        while Instant::now() < deadline {
            if ready(self) {
                return true;
            }
            let received = self.receive(deadline).expect("owned fixture IPC");
            assert!(
                !self.has("EXPIRED") && !self.has("IPC_ERROR") && !self.has("WORKER_EXIT PANIC"),
                "invalid worker outcome: {:?}",
                self.events
            );
            if self.mode == "open" && self.has("ENTERED 1") && self.release.is_none() {
                self.send_release(false).expect("open parent release");
            }
            if !received && self.poll_status().expect("child status") {
                return ready(self);
            }
        }
        ready(self)
    }

    fn cleanup(&mut self) {
        if self.cleanup_started {
            return;
        }
        self.cleanup_started = true;
        let deadline = (Instant::now() + SETTLE).min(self.deadline);
        if let Err(error) = self.poll_status() {
            self.cleanup_failure(&format!("initial status: {error}"));
        }
        if self.status.is_some() {
            self.drain_exited(deadline);
            return;
        }
        if let Err(error) = self.send_release(true) {
            match self.poll_status() {
                Ok(true) => {}
                Ok(false) => self.cleanup_failure(&format!("release before reap: {error}")),
                Err(status_error) => {
                    self.cleanup_failure(&format!("release: {error}; status: {status_error}"))
                }
            }
        }
        while Instant::now() < deadline {
            if let Err(error) = self.receive_for_exit(deadline) {
                self.cleanup_failure(&format!("IPC: {error}"));
            }
            match self.poll_status() {
                Ok(true) => {
                    self.drain_exited(deadline);
                    return;
                }
                Ok(false) => {}
                Err(error) => {
                    self.cleanup_failure(&format!("status: {error}"));
                    break;
                }
            }
        }
        self.cleanup_failure("normal cleanup exhausted; recorded before owned child kill");
        if let Err(error) = self.child.kill() {
            self.cleanup_failure(&format!("kill: {error}"));
        }
        let reap_deadline = (Instant::now() + SETTLE).min(self.deadline);
        loop {
            match self.poll_status() {
                Ok(true) => {
                    self.drain_exited(reap_deadline);
                    return;
                }
                Ok(false) => {}
                Err(error) => {
                    self.cleanup_failure(&format!("final try_wait: {error}"));
                    break;
                }
            }
            if Instant::now() >= reap_deadline {
                break;
            }
            if let Err(error) = self.receive_for_exit(reap_deadline) {
                self.cleanup_failure(&format!("final IPC drain: {error}"));
            }
        }
        self.cleanup_failure("child remains unreaped at final deadline; private path retained");
    }

    fn cleanup_failure(&mut self, reason: &str) {
        self.cleanup_ok = false;
        eprintln!("CLEANUP_FAILURE child {}: {reason}", self.child.id());
    }

    fn drain_exited(&mut self, deadline: Instant) {
        while !self.terminal_ipc_closed && Instant::now() < deadline {
            match self.receive_for_exit(deadline) {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => {
                    self.cleanup_failure(&format!("post-exit IPC drain: {error}"));
                    break;
                }
            }
        }
    }

    fn output(&self) -> String {
        let path = self.directory.as_ref().expect("owned directory").path();
        format!(
            "{}\n{}",
            log_excerpt(&path.join("stdout")).expect("child stdout"),
            log_excerpt(&path.join("stderr")).expect("child stderr")
        )
    }

    fn close_directory(&mut self) {
        assert!(
            self.status.is_some(),
            "never remove an unreaped child's owned path"
        );
        self.directory
            .take()
            .expect("owned directory")
            .close()
            .expect("owned path cleanup");
    }
}

impl Drop for ChildRun {
    fn drop(&mut self) {
        self.cleanup();
        if let Some(directory) = self.directory.take() {
            if self.status.is_some() {
                if let Err(error) = directory.close() {
                    eprintln!(
                        "CLEANUP_FAILURE child {} owned directory: {error}",
                        self.child.id()
                    );
                }
            } else {
                eprintln!(
                    "CLEANUP_FAILURE child {} unreaped; retained private path {}",
                    self.child.id(),
                    directory.path().display()
                );
                // Preserve evidence and an active child's owned paths on fixture failure.
                std::mem::forget(directory);
            }
        }
    }
}

fn log_excerpt(path: &Path) -> io::Result<String> {
    const LIMIT: u64 = 64 * 1024;
    const HALF: u64 = LIMIT / 2;
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut head = Vec::new();
    if length <= LIMIT {
        (&mut file).take(LIMIT).read_to_end(&mut head)?;
        let suffix = if file.metadata()?.len() > LIMIT {
            "\n[TRUNCATED: log grew beyond the bounded prefix]"
        } else {
            ""
        };
        return Ok(format!("{}{suffix}", String::from_utf8_lossy(&head)));
    }
    (&mut file).take(HALF).read_to_end(&mut head)?;
    file.seek(SeekFrom::End(-(HALF as i64)))?;
    let mut tail = Vec::new();
    (&mut file).take(HALF).read_to_end(&mut tail)?;
    Ok(format!(
        "{}\n[TRUNCATED: bounded head/tail of {length}-byte log]\n{}",
        String::from_utf8_lossy(&head),
        String::from_utf8_lossy(&tail)
    ))
}

fn require_counts(run: &mut ChildRun) {
    let (captures, entries) = run.counts().expect("request returned its counts");
    if captures != 1 || entries != 1 {
        run.cleanup();
        let output = run.output();
        assert!(
            output.contains("running 1 test"),
            "exact_child_test_count: {output}"
        );
        assert!(
            run.cleanup_ok && run.has("POST_DROP"),
            "control cleanup: {output}"
        );
        eprintln!("6339 missing-hook child output: {output}");
        run.close_directory();
    }
    assert_eq!(captures, 1, "actual_build_gate_capture_count");
    assert_eq!(entries, 1, "actual_build_gate_entry_count");
}

fn run_case(mode: &'static str) {
    run_case_with_peer_close(mode, None);
}

fn run_case_with_peer_close(mode: &'static str, injected_close: Option<InjectedPeerClose>) {
    let mut run = ChildRun::spawn(mode);
    assert!(
        run.collect_until(SETUP, |run| run.has("REQUEST_RETURNED")
            && run.counts().is_some()),
        "request/setup guard exhausted: {:?}",
        run.events
    );
    require_counts(&mut run);
    assert!(
        run.has(&format!("CHILD_START {mode}")),
        "selected exact child"
    );
    assert_eq!(
        run.events
            .iter()
            .filter(|event| *event == "CAPTURE 1")
            .count(),
        1
    );
    assert_eq!(
        run.events
            .iter()
            .filter(|event| *event == "ENTERED 1")
            .count(),
        1
    );
    assert!(
        run.collect_until(SETUP, |run| run.has("PRE_DROP")),
        "PRE_DROP guard"
    );

    if mode == "held" {
        assert!(run.has("FALLBACK_OK"), "bound readiness fallback");
        let end = Instant::now() + HELD;
        assert!(
            end <= run.deadline,
            "full held observation exceeds child watchdog"
        );
        while Instant::now() < end {
            run.receive(end).expect("held observation IPC");
            // This records the current unbounded teardown behavior.
            // A bounded teardown fix must flip this held-arm expectation.
            assert!(
                !run.has("EXPIRED")
                    && !run.has("IPC_ERROR")
                    && !run.has("WORKER_EXIT PANIC")
                    && !run.has("WORKER_EXIT RETURN")
                    && !run.has("RELEASED NORMAL")
                    && !run.has("RELEASED CLEANUP")
                    && !run.has("POST_DROP"),
                "held worker/drop completed before parent release: {:?}",
                run.events
            );
            assert!(
                !run.poll_status().expect("held child status"),
                "held child exited"
            );
        }
        eprintln!("C1_HELD_OBSERVED: ENTERED + FALLBACK_OK + PRE_DROP; no expiry/exit/drop");
        let normal_release = run.send_release(false);
        if normal_release.is_err() {
            run.cleanup();
            let output = run.output();
            assert!(run.cleanup_ok, "control cleanup failed: {output}");
            assert!(run.has("RELEASED CLEANUP") && run.has("WORKER_EXIT RETURN"));
            assert!(run.has("POST_DROP") && run.has("CHILD_OK"));
            assert!(run.status.as_ref().is_some_and(|status| status.success()));
            assert!(
                output.contains("running 1 test"),
                "exact_child_test_count: {output}"
            );
            run.close_directory();
        }
        assert!(normal_release.is_ok(), "normal_parent_release_required");
        let released_at = normal_release.expect("normal parent release");
        assert!(
            run.collect_until(SETTLE, |run| {
                run.has("RELEASED NORMAL") && run.has("WORKER_EXIT RETURN") && run.has("POST_DROP")
            }),
            "post-release completion guard"
        );
        eprintln!(
            "release-correlated POST_DROP observed after {:?}",
            released_at.elapsed()
        );
    } else {
        assert!(
            run.has("COLD_OK") && run.has("WARM_OK"),
            "open positive route"
        );
    }

    // Fault scheduling changes only when the synthetic I/O result is delivered.
    // Classification always uses the actually collected receipts.
    let (injected_close_consumed, premature_close_error) = if let Some(fault) = injected_close {
        assert!(
            run.collect_until(SETTLE, |run| {
                run.has("POST_DROP") && (fault.before_child_ok || run.has("CHILD_OK"))
            }),
            "peer_close_injection_stage_guard"
        );
        assert_eq!(
            run.has("CHILD_OK"),
            !fault.before_child_ok,
            "peer_close_injection_position"
        );
        assert!(
            !run.terminal_ipc_closed,
            "peer_close_injection_not_already_closed"
        );
        eprintln!(
            "INJECTED_PEER_CLOSE before_child_ok={} kind={:?}",
            fault.before_child_ok, fault.kind
        );
        let result = run.accept_exit_receive(Err(io::Error::new(
            fault.kind,
            "injected terminal peer close",
        )));
        (true, result.err())
    } else {
        (false, None)
    };

    assert!(
        run.collect_until(SETTLE, |run| run.has("CHILD_OK") && run.has("POST_DROP")),
        "normal child completion guard"
    );
    let end = (Instant::now() + SETTLE).min(run.deadline);
    while !run.poll_status().expect("normal child status") && Instant::now() < end {
        run.receive_for_exit(end).expect("normal child IPC");
    }
    let output = run.output();
    assert!(run.status.is_some(), "normal child exit guard: {output}");
    run.cleanup();
    assert!(run.cleanup_ok, "normal child cleanup: {output}");
    assert!(
        run.status.as_ref().is_some_and(|status| status.success()),
        "normal exit/reap: {output}"
    );
    assert!(
        output.contains("running 1 test"),
        "exact_child_test_count: {output}"
    );
    assert!(run.has("RELEASED NORMAL") && run.has("WORKER_EXIT RETURN"));
    run.close_directory();
    if injected_close.is_some() {
        assert!(injected_close_consumed, "injected_peer_close_consumed");
        // The rejection is retained even if cleanup later collected CHILD_OK.
        assert!(
            premature_close_error.is_none(),
            "premature_terminal_ipc_close: {premature_close_error:?}"
        );
        assert!(run.terminal_ipc_closed, "qualified_peer_close_recorded");
    }
}

#[test]
fn actual_cold_miss_owned_runtime_release_witness() {
    run_case("open");
    run_case("held");
}

#[test]
fn terminal_peer_close_after_child_ok_is_accepted() {
    // Real cold/warm and held/release positives precede fault sensitivity.
    run_case("open");
    run_case("held");
    let before_child_ok = false;
    for kind in [io::ErrorKind::ConnectionReset, io::ErrorKind::UnexpectedEof] {
        run_case_with_peer_close(
            "open",
            Some(InjectedPeerClose {
                before_child_ok,
                kind,
            }),
        );
    }
}

#[test]
#[ignore = "selected only by the owned exact-test parent"]
fn cold_miss_child() {
    let root = PathBuf::from(std::env::var_os(ROOT).expect("owned fixture root"));
    let nonce = std::env::var(NONCE).expect("parent nonce");
    let mode = std::env::var(MODE).expect("child mode");
    assert!(matches!(mode.as_str(), "open" | "held"));
    let ipc = gate::Ipc::connect(&root.join("c"), &root.join("p"), nonce.clone(), FAILSAFE)
        .expect("child Unix IPC");
    ipc.send(&format!("CHILD_START {mode}"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("explicitly owned Tokio runtime");
    let installation = runtime.block_on(async {
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(root.join("fixture.db")),
            embedding_model: None,
            additional_embedding_models: vec![],
            ..RuntimeConfig::default()
        })
        .expect("private runtime");
        rt.register_embedder(HashVecProvider {
            model_name: "ann-route-test-model".to_owned(),
            dims: 32,
        });
        let token = rt
            .authorize(Namespace::parse("local").expect("local"))
            .expect("authorize");
        for i in 0..32u32 {
            rt.create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("ann warm route note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("private note");
        }
        let pack = MemoryPack::new(rt.clone());
        let ann = pack.ann_for_test();
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(pack);
        let registry = builder.build().expect("private registry");
        let installation = gate::install(ipc.clone());
        let query = if mode == "held" {
            format!("nofts6339{nonce}")
        } else {
            "ann warm route note 7".to_owned()
        };
        let response = registry
            .dispatch_with_disposition(
                "memory.recall",
                serde_json::json!({"query": query, "limit": 10}),
                None,
            )
            .await
            .expect("actual cold recall");
        if mode == "open" {
            ipc.send("COLD_OK");
            ann.reset_warm_route_count();
            registry
                .dispatch_with_disposition(
                    "memory.recall",
                    serde_json::json!({"query": "ann warm route note 7", "limit": 10}),
                    None,
                )
                .await
                .expect("second recall");
            assert!(ann.warm_route_count() > 0, "second recall uses warm ANN");
            ipc.send("WARM_OK");
        }
        let (captures, entries) = installation.counts();
        ipc.send(&format!("COUNTS {captures} {entries}"));
        if mode == "held" && entries == 1 {
            assert!(
                !installation.released(),
                "fallback while worker remains held"
            );
            assert_eq!(
                response.get("degraded").and_then(|v| v.as_bool()),
                Some(true)
            );
            assert!(response
                .get("results")
                .and_then(|v| v.as_array())
                .is_some_and(|v| v.is_empty()));
            assert!(response
                .get("degraded_reason")
                .and_then(|v| v.as_str())
                .is_some_and(|v| !v.is_empty()));
            ipc.send("FALLBACK_OK");
        }
        ipc.send("REQUEST_RETURNED");
        installation
    });
    ipc.send("PRE_DROP");
    drop(runtime);
    ipc.send("POST_DROP");
    let (captures, entries) = installation.counts();
    assert_eq!(captures, 1, "actual_build_gate_capture_count");
    assert_eq!(entries, 1, "actual_build_gate_entry_count");
    ipc.send("CHILD_OK");
}
