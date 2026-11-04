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
use gate::protocol::{self, ReceivedCase, Release};

const SETUP: Duration = Duration::from_secs(30);
const HELD: Duration = Duration::from_secs(5);
const SETTLE: Duration = Duration::from_secs(20);
const FAILSAFE: Duration = Duration::from_secs(60);
const WATCHDOG: Duration = Duration::from_secs(90);
const POLL: Duration = Duration::from_millis(20);
const HELD_READINESS: Duration = Duration::from_secs(1);
const OPEN_READINESS_MARGIN: Duration = Duration::from_secs(5);

fn assert_timing_contract() {
    assert!(!POLL.is_zero() && POLL < HELD, "timing_poll_contract");
    assert!(HELD_READINESS < SETUP, "timing_readiness_contract");
    assert!(SETUP + OPEN_READINESS_MARGIN > SETUP, "timing_open_margin");
    assert!(HELD < SETTLE, "timing_live_cleanup_contract");
    assert!(
        HELD_READINESS + HELD + SETTLE < FAILSAFE,
        "timing_gate_failsafe"
    );
    assert!(SETUP + FAILSAFE <= WATCHDOG, "timing_shared_watchdog");
    // Phases share this shorter absolute cap; their maxima are not additive promises.
    let phase_sum = SETUP + SETUP + HELD + SETTLE + SETTLE + SETTLE;
    assert!(WATCHDOG < phase_sum, "timing_shared_phase_cap");
}

const ROOT: &str = "KHIVE_TEST_MEMORY_ANN_ROOT";
const NONCE: &str = "KHIVE_TEST_MEMORY_ANN_NONCE";
const MODE: &str = "KHIVE_TEST_MEMORY_ANN_MODE";

#[derive(Debug)]
enum ChildLifecycle {
    Spawned,
    Connected,
    Released,
    Reaped(ExitStatus),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CleanupState {
    Pending,
    Running,
    Passed,
    Failed,
}

struct ChildRun {
    directory: Option<tempfile::TempDir>,
    child: Child,
    socket: UnixDatagram,
    child_socket: PathBuf,
    nonce: String,
    mode: &'static str,
    events: Vec<String>,
    lifecycle: ChildLifecycle,
    cleanup: CleanupState,
    deadline: Instant,
}

impl ChildRun {
    fn spawn(mode: &'static str) -> Self {
        assert_timing_contract();
        let directory = tempfile::Builder::new()
            .prefix("ann-drop-")
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
        let readiness = if mode == protocol::HELD {
            HELD_READINESS
        } else {
            SETUP + OPEN_READINESS_MARGIN
        };
        let readiness_ms = readiness.as_millis().to_string();
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
            lifecycle: ChildLifecycle::Spawned,
            cleanup: CleanupState::Pending,
            deadline,
        }
    }

    fn has(&self, event: &str) -> bool {
        self.events.iter().any(|seen| seen == event)
    }

    fn counts(&self) -> Option<(usize, usize)> {
        let event = self.events.iter().find_map(|event| {
            event
                .strip_prefix(protocol::COUNTS)
                .and_then(|event| event.strip_prefix(' '))
        })?;
        let (captures, entries) = event.split_once(' ')?;
        Some((captures.parse().ok()?, entries.parse().ok()?))
    }

    fn mapping_result(&self, case: ReceivedCase) -> Option<(bool, bool)> {
        let prefix = format!("{} {} ", protocol::MAPPING, case.variant());
        self.events.iter().find_map(|event| {
            let rest = event.strip_prefix(&prefix)?;
            let (valid, received) = rest.split_once(' ')?;
            Some((valid.parse().ok()?, received.parse().ok()?))
        })
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
        protocol::validate(event)?;
        if matches!(&self.lifecycle, ChildLifecycle::Spawned) {
            self.socket.connect(&self.child_socket)?;
            self.lifecycle = ChildLifecycle::Connected;
        }
        eprintln!("memory ANN {}: {event}", self.mode);
        self.events.push(event.to_owned());
        Ok(true)
    }

    fn send_release(&mut self, release: Release) -> io::Result<Instant> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "child watchdog reached",
            ));
        }
        if !matches!(
            &self.lifecycle,
            ChildLifecycle::Connected | ChildLifecycle::Released
        ) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "child sender not authenticated",
            ));
        }
        self.socket.set_write_timeout(Some(POLL.min(remaining)))?;
        let event = release.command();
        let frame = format!("{} {event}", self.nonce);
        let sent = self.socket.send(frame.as_bytes())?;
        let delivered_at = Instant::now();
        if sent != frame.len() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "parent IPC frame"));
        }
        self.lifecycle = ChildLifecycle::Released;
        eprintln!("memory ANN {} parent: {event}", self.mode);
        Ok(delivered_at)
    }

    fn poll_status(&mut self) -> io::Result<bool> {
        if self.status().is_none() {
            if let Some(status) = self.child.try_wait()? {
                self.lifecycle = ChildLifecycle::Reaped(status);
                eprintln!(
                    "memory ANN {} parent: REAPED {:?}",
                    self.mode,
                    self.status()
                );
            }
        }
        Ok(self.status().is_some())
    }

    fn collect_until(&mut self, budget: Duration, ready: impl Fn(&Self) -> bool) -> bool {
        let deadline = (Instant::now() + budget).min(self.deadline);
        while Instant::now() < deadline {
            if ready(self) {
                return true;
            }
            let received = self.receive(deadline).expect("owned fixture IPC");
            assert!(!self.has(protocol::EXPIRED), "body_gate_failsafe_expired");
            assert!(!self.has(protocol::IPC_ERROR), "body_gate_ipc_error");
            assert!(!self.has(protocol::BODY_PANIC), "body_scope_panicked");
            if self.mode != protocol::HELD
                && self.has(protocol::ENTERED_ONCE)
                && matches!(&self.lifecycle, ChildLifecycle::Connected)
            {
                self.send_release(Release::Normal)
                    .expect("open parent release");
            }
            if !received && self.poll_status().expect("child status") {
                return ready(self);
            }
        }
        ready(self)
    }

    fn status(&self) -> Option<&ExitStatus> {
        match &self.lifecycle {
            ChildLifecycle::Reaped(status) => Some(status),
            _ => None,
        }
    }

    fn cleanup_ok(&self) -> bool {
        self.cleanup == CleanupState::Passed
    }

    fn finish_cleanup(&mut self) {
        if self.cleanup == CleanupState::Running {
            self.cleanup = CleanupState::Passed;
        }
    }

    fn cleanup(&mut self) {
        if self.cleanup != CleanupState::Pending {
            return;
        }
        self.cleanup = CleanupState::Running;
        let deadline = (Instant::now() + SETTLE).min(self.deadline);
        let mut release_attempted = false;
        loop {
            match self.poll_status() {
                Ok(true) => {
                    self.drain_exited(deadline);
                    self.finish_cleanup();
                    return;
                }
                Ok(false) => {}
                Err(error) => self.cleanup_failure(&format!("status: {error}")),
            }
            if !release_attempted
                && matches!(
                    &self.lifecycle,
                    ChildLifecycle::Connected | ChildLifecycle::Released
                )
            {
                release_attempted = true;
                if let Err(error) = self.send_release(Release::Cleanup) {
                    match self.poll_status() {
                        Ok(true) => {}
                        _ => self.cleanup_failure(&format!("release: {error}")),
                    }
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            if let Err(error) = self.receive(deadline) {
                self.cleanup_failure(&format!("IPC: {error}"));
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
                    self.finish_cleanup();
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
            if let Err(error) = self.receive(reap_deadline) {
                self.cleanup_failure(&format!("final IPC drain: {error}"));
            }
        }
        self.cleanup_failure("child remains unreaped; private path retained");
    }

    fn cleanup_failure(&mut self, reason: &str) {
        self.cleanup = CleanupState::Failed;
        eprintln!("CLEANUP_FAILURE child {}: {reason}", self.child.id());
    }

    fn drain_exited(&mut self, deadline: Instant) {
        while Instant::now() < deadline {
            match self.receive(deadline) {
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
            self.status().is_some(),
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
        // Panic unwinding can wait through two SETTLE polling stages (at most 40s),
        // both clipped to the shared watchdog. No blocking Child::wait is used.
        self.cleanup();
        if let Some(directory) = self.directory.take() {
            if self.status().is_some() {
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
        assert!(run.cleanup_ok(), "control cleanup: {output}");
        assert!(
            run.has(protocol::POST_DROP) && run.has(protocol::CHILD_OK),
            "control child completion: {output}"
        );
        assert!(
            run.status().is_some_and(|status| status.success()),
            "control normal exit/reap: {output}"
        );
        if entries == 1 {
            assert!(
                run.has(protocol::BODY_CONFIRMED),
                "control body-scope confirmation: {output}"
            );
        }
        eprintln!("memory ANN missing-hook child output: {output}");
        run.close_directory();
    }
    assert_eq!(captures, 1, "actual_build_gate_capture_count");
    assert_eq!(entries, 1, "actual_build_gate_entry_count");
}

fn run_case(mode: &'static str) {
    let mut run = ChildRun::spawn(mode);
    let request_returned = run.collect_until(SETUP, |run| {
        run.has(protocol::REQUEST_RETURNED) && run.counts().is_some()
    });
    if !request_returned {
        eprintln!("memory ANN request-stage diagnostic: {:?}", run.events);
    }
    assert!(request_returned, "request_setup_guard");
    require_counts(&mut run);
    assert!(
        run.has(&format!("{} {mode}", protocol::START)),
        "selected exact child"
    );
    assert_eq!(
        run.events
            .iter()
            .filter(|event| *event == protocol::CAPTURE_ONCE)
            .count(),
        1
    );
    assert_eq!(
        run.events
            .iter()
            .filter(|event| *event == protocol::ENTERED_ONCE)
            .count(),
        1
    );
    let pre_drop = run.collect_until(SETUP, |run| run.has(protocol::PRE_DROP));
    if !pre_drop {
        eprintln!("memory ANN pre-drop diagnostic: {:?}", run.events);
    }
    assert!(pre_drop, "pre_drop_guard");

    // https://github.com/ohdearquant/khive/issues/6339 tracks this teardown dependency.
    // Keep the fixed hold only for causal ordering; these are fixture guards, not ANN SLAs.
    let mut held_observation = None;
    if mode == protocol::HELD {
        assert!(run.has(protocol::FALLBACK_OK), "bound readiness fallback");
        let end = Instant::now() + HELD;
        assert!(
            end <= run.deadline,
            "full held observation exceeds child watchdog"
        );
        while Instant::now() < end {
            run.receive(end).expect("held observation IPC");
            assert!(!run.has(protocol::EXPIRED), "body_gate_failsafe_expired");
            assert!(!run.has(protocol::IPC_ERROR), "body_gate_ipc_error");
            assert!(!run.has(protocol::BODY_PANIC), "body_scope_panicked");
            let released_or_returned = run.has(protocol::BODY_RETURN)
                || run.has(protocol::RELEASED_NORMAL)
                || run.has(protocol::RELEASED_CLEANUP)
                || run.has(protocol::BODY_CONFIRMED);
            if released_or_returned {
                eprintln!("memory ANN held-stage diagnostic: {:?}", run.events);
            }
            assert!(
                !released_or_returned,
                "body_scope_released_or_returned_early"
            );
            assert!(
                !run.poll_status().expect("held child status"),
                "held child exited"
            );
        }
        let post_drop_before_release = run.has(protocol::POST_DROP);
        eprintln!(
            "HELD_DROP_OBSERVED: entered/fallback/PRE_DROP; actual closure held; POST_DROP={post_drop_before_release}"
        );
        held_observation = Some((post_drop_before_release, run.has(protocol::BACKGROUND)));
        eprintln!(
            "C1_HELD_OBSERVED: pre-release POST_DROP recorded; body gate held and child alive"
        );
        let normal_release = run.send_release(Release::Normal);
        if normal_release.is_err() {
            run.cleanup();
            let output = run.output();
            assert!(run.cleanup_ok(), "control cleanup failed: {output}");
            assert!(run.has(protocol::RELEASED_CLEANUP) && run.has(protocol::BODY_RETURN));
            assert!(
                run.has(protocol::POST_DROP)
                    && run.has(protocol::BODY_CONFIRMED)
                    && run.has(protocol::CHILD_OK)
            );
            assert!(run.status().is_some_and(|status| status.success()));
            assert!(
                output.contains("running 1 test"),
                "exact_child_test_count: {output}"
            );
            run.close_directory();
            assert!(
                held_observation == Some((true, true)),
                "normal_release_control_preconditions"
            );
        }
        assert!(normal_release.is_ok(), "normal_parent_release_required");
        let released_at = normal_release.expect("normal parent release");
        assert!(
            run.collect_until(SETTLE, |run| {
                run.has(protocol::RELEASED_NORMAL)
                    && run.has(protocol::BODY_RETURN)
                    && run.has(protocol::BODY_CONFIRMED)
            }),
            "post_drop_body_scope_guard"
        );
        eprintln!(
            "post-drop body scope observed after RELEASE: {:?}",
            released_at.elapsed()
        );
    } else if let Some(case) = ReceivedCase::from_mode(mode) {
        assert!(run.has(protocol::REAL_SUBMIT), "real_submit_completed");
        assert!(
            run.has(&format!("{} {}", protocol::REPLACED, case.variant())),
            "received_replaced"
        );
        let (valid, received) = run.mapping_result(case).expect("mapping result receipt");
        if !valid || !received {
            run.cleanup();
            let output = run.output();
            eprintln!("memory ANN mapping diagnostic: {output}");
            assert!(run.cleanup_ok(), "mapping control cleanup");
            assert!(
                run.has(protocol::POST_DROP)
                    && run.has(protocol::BODY_CONFIRMED)
                    && run.has(protocol::CHILD_OK)
            );
            assert!(run.has(protocol::RELEASED_NORMAL) && run.has(protocol::BODY_RETURN));
            assert!(
                run.status().is_some_and(|status| status.success()),
                "mapping control normal exit/reap"
            );
            assert!(output.contains("running 1 test"), "exact_child_test_count");
            run.close_directory();
        }
        assert!(received, "received_value_not_elapsed_or_sender_loss");
        assert!(valid, "received_outcome_degraded");
        assert!(
            run.has(protocol::BACKGROUND),
            "actual_background_build_context"
        );
    } else {
        assert!(
            run.has(protocol::COLD_OK) && run.has(protocol::WARM_OK),
            "open positive route"
        );
    }

    let child_completed = run.collect_until(SETTLE, |run| {
        run.has(protocol::CHILD_OK)
            && run.has(protocol::POST_DROP)
            && run.has(protocol::BODY_CONFIRMED)
    });
    if !child_completed {
        eprintln!("memory ANN child-completion diagnostic: {:?}", run.events);
    }
    assert!(child_completed, "normal_child_completion_guard");
    let end = (Instant::now() + SETTLE).min(run.deadline);
    while !run.poll_status().expect("normal child status") && Instant::now() < end {
        run.receive(end).expect("normal child IPC");
    }
    let output = run.output();
    assert!(run.status().is_some(), "normal child exit guard: {output}");
    run.cleanup();
    assert!(run.cleanup_ok(), "normal child cleanup: {output}");
    assert!(
        run.status().is_some_and(|status| status.success()),
        "normal exit/reap: {output}"
    );
    assert!(
        output.contains("running 1 test"),
        "exact_child_test_count: {output}"
    );
    assert!(run.has(protocol::RELEASED_NORMAL) && run.has(protocol::BODY_RETURN));
    run.close_directory();
    if let Some((post_drop_before_release, held_background)) = held_observation {
        // Check the causal ordering after normal release/completion/reap in both routes.
        assert!(post_drop_before_release, "post_drop_before_parent_release");
        assert!(held_background, "actual_background_build_context");
    }
}

#[test]
fn actual_cold_miss_owned_runtime_release_witness() {
    run_case(protocol::OPEN);
    run_case(protocol::HELD);
}

#[test]
fn received_cancelled_outcome_is_degraded() {
    run_case(ReceivedCase::Cancelled.mode());
}

#[test]
fn received_refused_outcome_is_degraded() {
    run_case(ReceivedCase::Refused.mode());
}

#[test]
fn received_finalization_timeout_outcome_is_degraded() {
    run_case(ReceivedCase::FinalizationTimedOut.mode());
}

#[test]
#[ignore = "selected only by the owned exact-test parent"]
fn cold_miss_child() {
    assert_timing_contract();
    let root = PathBuf::from(std::env::var_os(ROOT).expect("owned fixture root"));
    let nonce = std::env::var(NONCE).expect("parent nonce");
    let mode = std::env::var(MODE).expect("child mode");
    let received_case = ReceivedCase::from_mode(&mode);
    assert!(matches!(mode.as_str(), protocol::OPEN | protocol::HELD) || received_case.is_some());
    let ipc = gate::Ipc::connect(&root.join("c"), &root.join("p"), nonce.clone(), FAILSAFE)
        .expect("child Unix IPC");
    ipc.send(&format!("{} {mode}", protocol::START));
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
        let installation = if received_case.is_some() {
            gate::install_received(ipc.clone(), received_case)
        } else {
            gate::install(ipc.clone())
        };
        let query = if mode != protocol::OPEN {
            format!("absentmemoryann{nonce}")
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
        if mode == protocol::OPEN {
            ipc.send(protocol::COLD_OK);
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
            ipc.send(protocol::WARM_OK);
        }
        if let Some(case) = received_case {
            let reason = response
                .get("degraded_reason")
                .and_then(|value| value.as_str());
            let valid = response.get("degraded").and_then(|value| value.as_bool()) == Some(true)
                && reason.is_some_and(|reason| reason.contains(case.variant()));
            let received = !reason.is_some_and(|reason| {
                reason.contains("memory ANN not ready within bounded wait")
                    || reason.contains("memory ANN detached build task ended without a result")
            });
            eprintln!(
                "memory ANN received {}: valid={valid}, received={received}, reason={reason:?}",
                case.variant()
            );
            ipc.send(&format!(
                "{} {} {valid} {received}",
                protocol::MAPPING,
                case.variant()
            ));
        }
        let (captures, entries) = installation.counts();
        ipc.send(&format!("{} {captures} {entries}", protocol::COUNTS));
        if mode == protocol::HELD && entries == 1 {
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
            ipc.send(protocol::FALLBACK_OK);
        }
        ipc.send(protocol::REQUEST_RETURNED);
        installation
    });
    ipc.send(protocol::PRE_DROP);
    drop(runtime);
    ipc.send(protocol::POST_DROP);
    let entries = installation.counts().1;
    if entries == 1 {
        // Independent live cleanup after runtime drop; no competing IPC RELEASE reader.
        assert!(
            installation.wait_body_scope_exit(SETTLE),
            "body-scope exit guard"
        );
        ipc.send(protocol::BODY_CONFIRMED);
    }
    // Receipts are data here: the parent asserts counts/mapping only after normal reap.
    ipc.send(protocol::CHILD_OK);
}
