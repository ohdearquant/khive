//! Real launcher/daemon process controls for ADR-185 Amendment 1.
//! All configuration, rendezvous files, logs and children belong to this fixture.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const LABEL: &str = "test:supervisor-lifecycle";
const START_LIMIT: Duration = Duration::from_secs(30);

struct Fixture {
    root: TempDir,
    home: PathBuf,
    config: PathBuf,
    marker: PathBuf,
    socket: PathBuf,
    pid_file: PathBuf,
    database: PathBuf,
}

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Only the exact child we spawned; never enumerate or kill host daemons.
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

impl Fixture {
    fn new() -> Self {
        // Keep Unix socket names below the platform path-length limit.
        let root = tempfile::Builder::new()
            .prefix("k-sup-")
            .tempdir_in("/tmp")
            .unwrap();
        let home = root.path().join("home");
        std::fs::create_dir_all(home.join(".khive")).unwrap();
        let config = root.path().join("config.toml");
        let database = root.path().join("db");
        let database_toml = serde_json::to_string(database.to_str().unwrap()).unwrap();
        std::fs::write(
            &config,
            format!(
                "[runtime]\npacks = [\"kg\"]\n[actor]\nid = {LABEL:?}\n[[backends]]\nname = \"main\"\nkind = \"sqlite\"\npath = {database_toml}\n[packs.kg]\nbackend = \"main\"\nno_embed = true\n"
            ),
        )
        .unwrap();
        Self {
            home,
            config,
            marker: root.path().join("m"),
            socket: root.path().join("s"),
            pid_file: root.path().join("p"),
            database,
            root,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("KHIVE_SOCKET", &self.socket)
            .env("KHIVE_PID", &self.pid_file)
            .env("KHIVE_LOCK", self.root.path().join("boot.lock"))
            .env(
                "KHIVE_RECOVERER_LOCK",
                self.root.path().join("recoverer.lock"),
            )
            .env("KHIVE_SUPERVISOR_MARKER", &self.marker)
            .env("KHIVE_EVENTS_SPLIT", "0")
            .env("KHIVE_TEST_HARNESS", "1")
            .env("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1")
            .current_dir(self.root.path())
            .stdin(Stdio::null());
        command
    }

    fn launch_command(&self, config: &Path) -> Command {
        self.launch_command_with_packs(config, &["kg"])
    }

    fn launch_command_with_packs(&self, config: &Path, packs: &[&str]) -> Command {
        self.launch_command_options(config, packs, "10", true)
    }

    fn launch_command_options(
        &self,
        config: &Path,
        packs: &[&str],
        interval: &str,
        no_embed: bool,
    ) -> Command {
        let mut command = self.command();
        command
            .args([
                "supervisor",
                "launch",
                "--label",
                LABEL,
                "--restart-interval-secs",
                interval,
                "--",
                "--config",
            ])
            .arg(config)
            .args(["--actor", LABEL]);
        if no_embed {
            command.arg("--no-embed");
        }
        for pack in packs {
            command.args(["--pack", pack]);
        }
        command
    }

    fn exec_command(&self) -> Command {
        let mut command = self.command();
        command
            .args(["exec", "stats()", "--config"])
            .arg(&self.config)
            .args(["--actor", LABEL]);
        command
    }

    fn plan_command(&self) -> Command {
        let mut command = self.command();
        command
            .args(["exec", "--plan", "stats()", "--config"])
            .arg(&self.config);
        command
    }

    fn direct_daemon_command(&self) -> Command {
        let mut command = self.command();
        command
            .args(["mcp", "--daemon", "--config"])
            .arg(&self.config)
            .args(["--actor", LABEL, "--pack", "kg"]);
        command
    }

    fn stub_command(&self, mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "supervisor_socket_stub_child", "--nocapture"])
            .env("KHIVE_SUPERVISOR_STUB_MODE", mode)
            .env("KHIVE_SUPERVISOR_STUB_SOCKET", &self.socket)
            .stdin(Stdio::null());
        command
    }

    fn spawn(&self, command: &mut Command, log_name: &str) -> (OwnedChild, PathBuf) {
        let log_path = self.root.path().join(log_name);
        let log = File::create(&log_path).unwrap();
        command
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log));
        (OwnedChild(command.spawn().unwrap()), log_path)
    }

    fn completed(&self, command: &mut Command, log_name: &str) -> (ExitStatus, String) {
        let (mut child, log_path) = self.spawn(command, log_name);
        let deadline = Instant::now() + START_LIMIT;
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                return (status, std::fs::read_to_string(log_path).unwrap());
            }
            assert!(
                Instant::now() < deadline,
                "command did not finish: {}",
                std::fs::read_to_string(&log_path).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn release(&self, label: &str) -> (ExitStatus, String) {
        self.completed(
            self.command()
                .args(["supervisor", "release", "--label", label]),
            "release.log",
        )
    }
}

// MUST-FAIL: publishing a helper/parent PID instead of the exec'd process cannot
// satisfy the peer-credential equality, even if a socket happens to exist.
#[tokio::test]
async fn supervisor_launcher_marker_pid_is_the_actual_socket_owner() {
    let fixture = Fixture::new();
    let (mut launcher, log_path) =
        fixture.spawn(&mut fixture.launch_command(&fixture.config), "launch.log");
    let pid = launcher.0.id();
    let deadline = tokio::time::Instant::now() + START_LIMIT;
    let stream = loop {
        if let Ok(stream) = tokio::net::UnixStream::connect(&fixture.socket).await {
            if std::fs::read_to_string(&fixture.pid_file)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok())
                == Some(pid)
            {
                break stream;
            }
        }
        assert!(
            launcher.0.try_wait().unwrap().is_none(),
            "launcher exited before bind: {}",
            std::fs::read_to_string(&log_path).unwrap_or_default()
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "launcher did not bind: {}",
            std::fs::read_to_string(&log_path).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let marker = std::fs::read_to_string(&fixture.marker).unwrap();
    assert_eq!(marker, format!("{LABEL}\n{pid}\n10\n"));
    assert_eq!(
        stream.peer_cred().unwrap().pid(),
        Some(i32::try_from(pid).unwrap())
    );
    assert_eq!(
        std::fs::read_to_string(&fixture.pid_file)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap(),
        pid
    );
    drop(stream);
    // Even a clean daemon stop leaves marker cleanup to the deployment's
    // release command. SIGKILL alone would not exercise daemon cleanup code.
    // SAFETY: this is the unreaped child returned by our own spawn above.
    assert_eq!(
        unsafe { libc::kill(i32::try_from(pid).unwrap(), libc::SIGTERM) },
        0
    );
    let stop_deadline = tokio::time::Instant::now() + START_LIMIT;
    loop {
        if let Some(status) = launcher.0.try_wait().unwrap() {
            assert!(status.success(), "daemon did not stop cleanly: {status}");
            break;
        }
        assert!(
            tokio::time::Instant::now() < stop_deadline,
            "daemon did not drain: {}",
            std::fs::read_to_string(&log_path).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !fixture.socket.exists(),
        "clean stop must remove its socket"
    );
    assert!(
        !fixture.pid_file.exists(),
        "clean stop must remove its pid file"
    );
    assert_eq!(std::fs::read_to_string(&fixture.marker).unwrap(), marker);
    let (status, log) = fixture.release(LABEL);
    assert!(status.success(), "release failed: {log}");
    assert!(!fixture.marker.exists());
}

// MUST-FAIL: leaving a same-label marker after CONFIG refusal blocks the next
// unmanaged client until the suppression bound, even though no job will restart.
#[test]
fn supervisor_config_refusal_releases_own_marker_without_opening_a_database() {
    for contents in [None, Some("not valid [toml")] {
        let fixture = Fixture::new();
        let bad_config = fixture.root.path().join("bad.toml");
        if let Some(contents) = contents {
            std::fs::write(&bad_config, contents).unwrap();
        }
        std::fs::write(&fixture.marker, format!("{LABEL}\n1\n10\n")).unwrap();
        let (status, log) =
            fixture.completed(&mut fixture.launch_command(&bad_config), "config.log");
        assert!(status.success(), "CONFIG refusal must exit zero: {log}");
        assert!(log.contains("CONFIG"), "must disclose refusal: {log}");
        assert!(
            log.contains(bad_config.to_str().unwrap()),
            "wrong config path: {log}"
        );
        let expected = if contents.is_some() {
            "config TOML parse error"
        } else {
            "the explicitly selected config file does not exist"
        };
        assert!(log.contains(expected), "wrong CONFIG refusal: {log}");
        assert!(
            !log.contains("missing field `backend`"),
            "invalid valid-config fixture: {log}"
        );
        assert!(!fixture.marker.exists());
        assert!(!fixture.socket.exists());
        assert!(!fixture.pid_file.exists());
        assert!(!fixture.database.exists());
    }
}

// MUST-FAIL: own-label ownership checks must protect a foreign marker on both
// publication and cleanup, including when the proposed launch has bad config.
#[test]
fn supervisor_foreign_marker_survives_launch_and_release_refusals() {
    let fixture = Fixture::new();
    let foreign = b"another-supervisor\n1\n7\n";
    std::fs::write(&fixture.marker, foreign).unwrap();
    for config in [
        fixture.config.clone(),
        fixture.root.path().join("absent.toml"),
    ] {
        let (_, log) = fixture.completed(&mut fixture.launch_command(&config), "foreign.log");
        assert!(log.contains("another-supervisor"), "{log}");
        assert_eq!(
            std::fs::read(&fixture.marker).unwrap().as_slice(),
            foreign.as_slice()
        );
        assert!(!fixture.socket.exists());
        assert!(!fixture.pid_file.exists());
        assert!(!fixture.database.exists());
    }
    let (status, log) = fixture.release(LABEL);
    assert!(!status.success(), "foreign release must refuse: {log}");
    assert_eq!(
        std::fs::read(&fixture.marker).unwrap().as_slice(),
        foreign.as_slice()
    );
}

fn assert_pack_refusal(fixture: &Fixture, command: &mut Command, expected: &str) {
    let (status, log) = fixture.completed(command, "pack-refusal.log");
    assert!(
        status.success(),
        "pack CONFIG refusal must exit zero: {log}"
    );
    assert!(
        log.contains("CONFIG"),
        "must disclose CONFIG refusal: {log}"
    );
    assert!(log.contains(expected), "wrong pack refusal: {log}");
    assert!(
        !fixture.marker.exists(),
        "pack refusal must release the marker: {log}"
    );
    assert!(!fixture.socket.exists());
    assert!(!fixture.pid_file.exists());
    assert!(
        !fixture.database.exists(),
        "pack refusal must precede database creation: {log}"
    );
}

#[test]
fn supervisor_unknown_cli_pack_refuses_before_publish_and_releases_prior_marker() {
    for prior_marker in [false, true] {
        let fixture = Fixture::new();
        if prior_marker {
            std::fs::write(&fixture.marker, format!("{LABEL}\n1\n10\n")).unwrap();
        }
        assert_pack_refusal(
            &fixture,
            &mut fixture.launch_command_with_packs(&fixture.config, &["not-a-pack"]),
            "unknown pack",
        );
    }
}

#[test]
fn supervisor_duplicate_cli_pack_refuses_before_publish_and_releases_prior_marker() {
    for prior_marker in [false, true] {
        let fixture = Fixture::new();
        if prior_marker {
            std::fs::write(&fixture.marker, format!("{LABEL}\n1\n10\n")).unwrap();
        }
        assert_pack_refusal(
            &fixture,
            &mut fixture.launch_command_with_packs(&fixture.config, &["kg", "kg"]),
            "duplicate pack \"kg\"",
        );
    }
}

#[test]
fn supervisor_unknown_environment_pack_releases_prior_marker() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.marker, format!("{LABEL}\n1\n10\n")).unwrap();
    let mut command = fixture.launch_command_with_packs(&fixture.config, &[]);
    command.env("KHIVE_PACKS", "not-a-pack");
    assert_pack_refusal(&fixture, &mut command, "unknown pack");
}

#[test]
fn supervisor_unknown_config_pack_releases_prior_marker() {
    let fixture = Fixture::new();
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        config.replace("packs = [\"kg\"]", "packs = [\"not-a-pack\"]"),
    )
    .unwrap();
    std::fs::write(&fixture.marker, format!("{LABEL}\n1\n10\n")).unwrap();
    assert_pack_refusal(
        &fixture,
        &mut fixture.launch_command_with_packs(&fixture.config, &[]),
        "unknown pack",
    );
}

#[test]
fn supervisor_missing_pack_dependency_releases_prior_marker() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.marker, format!("{LABEL}\n1\n10\n")).unwrap();
    assert_pack_refusal(
        &fixture,
        &mut fixture.launch_command_with_packs(&fixture.config, &["git"]),
        "requires",
    );
}

async fn socket_holder_pid(fixture: &Fixture) -> Option<u32> {
    let stream = tokio::net::UnixStream::connect(&fixture.socket)
        .await
        .ok()?;
    stream
        .peer_cred()
        .ok()?
        .pid()
        .and_then(|pid| u32::try_from(pid).ok())
}

async fn wait_for_holder(fixture: &Fixture, expected: Option<u32>) -> u32 {
    let deadline = tokio::time::Instant::now() + START_LIMIT;
    loop {
        if let Some(pid) = socket_holder_pid(fixture).await {
            if expected.is_none_or(|expected| expected == pid)
                && std::fs::read_to_string(&fixture.pid_file)
                    .ok()
                    .and_then(|value| value.trim().parse::<u32>().ok())
                    == Some(pid)
            {
                return pid;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "socket holder did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_pid_gone(pid: u32) {
    let deadline = tokio::time::Instant::now() + START_LIMIT;
    while khive_mcp::daemon::supervisor_pid_is_alive(pid) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "pid {pid} did not leave"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn wait_child_exit(child: &mut OwnedChild, log_path: &Path) -> (ExitStatus, String) {
    let deadline = Instant::now() + START_LIMIT;
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            return (status, std::fs::read_to_string(log_path).unwrap());
        }
        assert!(
            Instant::now() < deadline,
            "child did not exit: {}",
            std::fs::read_to_string(log_path).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn is_handover_interrupted_stats(log: &str) -> bool {
    // A client already reading from the unmanaged daemon may receive its
    // cancelled read when the launcher drains that daemon. Accept only the
    // observed one-op stats response, never an arbitrary CLI failure.
    log.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .any(|response| {
            if response["status"] != "partial"
                || response["summary"]["total"] != 1
                || response["summary"]["failed"] != 1
                || response["summary"]["succeeded"] != 0
                || response["summary"]["aborted"] != 0
            {
                return false;
            }
            let Some(results) = response["results"].as_array() else {
                return false;
            };
            if results.len() != 1
                || results[0]["tool"] != "stats"
                || results[0]["ok"] != false
                || results[0]["error"]["kind"] != "runtime_error"
            {
                return false;
            }
            matches!(
                results[0]["error"]["message"].as_str(),
                Some("storage: timeout during count_entities")
                    | Some("storage: timeout during count_notes_in_namespaces")
            )
        })
}

/// This test body also serves as a subprocess-only Unix socket holder. The
/// parent launches the exact test name with an environment-selected mode.
#[test]
fn supervisor_socket_stub_child() {
    let Ok(mode) = std::env::var("KHIVE_SUPERVISOR_STUB_MODE") else {
        return;
    };
    let socket = PathBuf::from(std::env::var_os("KHIVE_SUPERVISOR_STUB_SOCKET").unwrap());
    if mode == "ignore-term" {
        // SAFETY: this subprocess is the isolated test stub and intentionally
        // ignores TERM to exercise the launcher's bounded refusal.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    }
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    for incoming in listener.incoming() {
        let Ok(mut stream) = incoming else { continue };
        let mode = mode.clone();
        std::thread::spawn(move || {
            if mode == "silent" {
                std::thread::sleep(Duration::from_secs(60));
                return;
            }
            let mut length = [0_u8; 4];
            if stream.read_exact(&mut length).is_err() {
                return;
            }
            let mut frame = vec![0_u8; u32::from_be_bytes(length) as usize];
            if stream.read_exact(&mut frame).is_err() {
                return;
            }
            let response = serde_json::json!({
                "ok": false,
                "result": null,
                "error": "stub configuration mismatch",
                "namespace_mismatch": false,
                "config_mismatch": true,
                "served_config_id": "stub-config",
                "version_mismatch": false,
                "daemon_protocol_version": khive_runtime::daemon::PROTOCOL_VERSION
            });
            let response = serde_json::to_vec(&response).unwrap();
            stream
                .write_all(&(response.len() as u32).to_be_bytes())
                .unwrap();
            stream.write_all(&response).unwrap();
        });
    }
}

fn wait_stub_socket(fixture: &Fixture, stub: &mut OwnedChild, log_path: &Path) {
    let deadline = Instant::now() + START_LIMIT;
    while !fixture.socket.exists() {
        assert!(
            stub.0.try_wait().unwrap().is_none(),
            "stub exited: {}",
            std::fs::read_to_string(log_path).unwrap_or_default()
        );
        assert!(Instant::now() < deadline, "stub did not bind");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// MUST-FAIL: removing the post-publish handover leaves the client-started
// daemon holding the socket and the supervised job refused by first writer.
#[tokio::test]
async fn supervisor_client_first_handover_and_legacy_control() {
    let fixture = Fixture::new();
    let (status, log) = fixture.completed(&mut fixture.exec_command(), "client-first.log");
    assert!(status.success(), "client auto-spawn failed: {log}");
    let old_pid = wait_for_holder(&fixture, None).await;
    let (mut launcher, launcher_log) = fixture.spawn(
        &mut fixture.launch_command_options(&fixture.config, &["kg"], "10", false),
        "handover.log",
    );
    let job_pid = launcher.0.id();
    assert_ne!(old_pid, job_pid);
    assert_eq!(wait_for_holder(&fixture, Some(job_pid)).await, job_pid);
    wait_pid_gone(old_pid).await;
    assert_eq!(
        std::fs::read_to_string(&fixture.marker).unwrap(),
        format!("{LABEL}\n{job_pid}\n10\n")
    );
    let (status, log) = fixture.completed(&mut fixture.exec_command(), "client-again.log");
    assert!(
        status.success(),
        "next request was not served by supervised configuration: {log}"
    );
    let (status, log) = fixture.completed(&mut fixture.plan_command(), "client-plan.log");
    assert!(
        status.success(),
        "supervised config did not answer a daemon-only plan: {log}"
    );
    assert_eq!(socket_holder_pid(&fixture).await, Some(job_pid));
    assert!(
        launcher.0.try_wait().unwrap().is_none(),
        "supervised daemon exited: {}",
        std::fs::read_to_string(launcher_log).unwrap_or_default()
    );
    assert!(
        std::fs::read_to_string(&launcher_log)
            .unwrap_or_default()
            .contains("supervisor replaced client-started incumbent"),
        "launcher did not log the replacement"
    );

    // Pre-A2 control in this same test file: publish the declaration, then
    // start the old direct daemon path without the launcher's handover.
    let control = Fixture::new();
    let (status, log) = control.completed(&mut control.exec_command(), "control-client.log");
    assert!(status.success(), "control client auto-spawn failed: {log}");
    let control_holder = wait_for_holder(&control, None).await;
    let (mut legacy_job, legacy_log) =
        control.spawn(&mut control.direct_daemon_command(), "legacy-job.log");
    let legacy_pid = legacy_job.0.id();
    std::fs::write(&control.marker, format!("{LABEL}\n{legacy_pid}\n10\n")).unwrap();
    let (status, log) = wait_child_exit(&mut legacy_job, &legacy_log);
    assert!(
        !status.success(),
        "pre-handover job unexpectedly served: {log}"
    );
    assert!(
        log.contains("already serving this socket"),
        "wrong first-writer refusal: {log}"
    );
    assert_ne!(socket_holder_pid(&control).await, Some(legacy_pid));
    assert_eq!(socket_holder_pid(&control).await, Some(control_holder));
    // SAFETY: this PID came from the fixture's own socket peer credentials.
    assert_eq!(
        unsafe { libc::kill(i32::try_from(control_holder).unwrap(), libc::SIGTERM) },
        0
    );
    wait_pid_gone(control_holder).await;
}

// Run explicitly in the hosted gate: cargo test -p kkernel --test
// supervisor_lifecycle supervisor_started_together_twenty_offsets -- --ignored --exact
// Each isolated fixture draws a fresh offset in [0, 1s), including both
// launch-first and client-first orders.
#[tokio::test]
#[ignore]
async fn supervisor_started_together_twenty_offsets() {
    for run in 0_u64..20 {
        let fixture = Fixture::new();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
        let offset = Duration::from_millis((nanos.wrapping_mul(791) + run * 53) % 1000);
        let mut launcher_command =
            fixture.launch_command_options(&fixture.config, &["kg"], "10", false);
        let (mut launcher, log_path) = if run % 2 == 0 {
            let (launcher, log_path) = fixture.spawn(&mut launcher_command, "together-launch.log");
            tokio::time::sleep(offset).await;
            let (status, log) =
                fixture.completed(&mut fixture.exec_command(), "together-client.log");
            assert!(status.success(), "run {run}: client failed: {log}");
            (launcher, log_path)
        } else {
            let (mut client, client_log) =
                fixture.spawn(&mut fixture.exec_command(), "together-client.log");
            tokio::time::sleep(offset).await;
            let overlapped = client.0.try_wait().unwrap().is_none();
            let (launcher, log_path) = fixture.spawn(&mut launcher_command, "together-launch.log");
            let (status, log) = wait_child_exit(&mut client, &client_log);
            assert!(
                status.success() || (overlapped && is_handover_interrupted_stats(&log)),
                "run {run}: client failed outside handover: {log}"
            );
            (launcher, log_path)
        };
        let job_pid = launcher.0.id();
        assert_eq!(
            wait_for_holder(&fixture, Some(job_pid)).await,
            job_pid,
            "run {run}"
        );
        let (status, log) =
            fixture.completed(&mut fixture.exec_command(), "together-fresh-client.log");
        assert!(
            status.success(),
            "run {run}: fresh request was not served by supervised daemon: {log}"
        );
        assert_eq!(
            socket_holder_pid(&fixture).await,
            Some(job_pid),
            "run {run}"
        );
        assert!(
            launcher.0.try_wait().unwrap().is_none(),
            "run {run}: launcher exited: {}",
            std::fs::read_to_string(log_path).unwrap_or_default()
        );
    }
}

// MUST-FAIL: removing the own-job guard overwrites the marker and sends TERM
// to the daemon already serving this same supervisor job.
#[tokio::test]
async fn supervisor_second_launch_leaves_own_job_and_marker_unchanged() {
    let fixture = Fixture::new();
    let (mut first, log_path) = fixture.spawn(
        &mut fixture.launch_command(&fixture.config),
        "own-first.log",
    );
    let first_pid = first.0.id();
    assert_eq!(wait_for_holder(&fixture, Some(first_pid)).await, first_pid);
    let marker = std::fs::read(&fixture.marker).unwrap();
    let (status, log) = fixture.completed(
        &mut fixture.launch_command(&fixture.config),
        "own-second.log",
    );
    assert!(!status.success(), "duplicate launcher must refuse: {log}");
    assert!(
        log.contains("duplicate supervisor launch"),
        "wrong duplicate refusal: {log}"
    );
    assert_eq!(std::fs::read(&fixture.marker).unwrap(), marker);
    assert_eq!(socket_holder_pid(&fixture).await, Some(first_pid));
    assert!(
        first.0.try_wait().unwrap().is_none(),
        "first job was disturbed: {}",
        std::fs::read_to_string(log_path).unwrap_or_default()
    );
    let bad_config = fixture.root.path().join("invalid.toml");
    std::fs::write(&bad_config, "not valid [toml").unwrap();
    let (status, log) =
        fixture.completed(&mut fixture.launch_command(&bad_config), "own-invalid.log");
    assert!(
        !status.success(),
        "live own job must win before config refusal: {log}"
    );
    assert!(
        log.contains("duplicate supervisor launch"),
        "wrong own-job refusal: {log}"
    );
    assert_eq!(std::fs::read(&fixture.marker).unwrap(), marker);
}

// MUST-FAIL: an accepted but silent socket does not grant permission to
// signal its peer, even though the peer is under the same uid.
#[test]
fn supervisor_silent_same_uid_stub_is_not_signalled() {
    let fixture = Fixture::new();
    let (mut stub, stub_log) =
        fixture.spawn(&mut fixture.stub_command("silent"), "silent-stub.log");
    wait_stub_socket(&fixture, &mut stub, &stub_log);
    let (status, log) = fixture.completed(
        &mut fixture.launch_command(&fixture.config),
        "silent-launch.log",
    );
    assert!(!status.success(), "silent socket must refuse: {log}");
    assert!(
        log.contains("incumbent is not a khive daemon"),
        "wrong refusal: {log}"
    );
    assert!(
        log.contains(&stub.0.id().to_string()),
        "missing peer pid: {log}"
    );
    assert!(log.contains("uid="), "missing uid: {log}");
    assert!(log.contains("waited="), "missing elapsed time: {log}");
    assert!(
        fixture.marker.exists(),
        "failed handover must retain marker"
    );
    assert!(
        stub.0.try_wait().unwrap().is_none(),
        "silent stub was signalled"
    );

    // Preserve an own-label declaration if its socket accepts but cannot
    // complete an identity response right now.
    let own = Fixture::new();
    let (mut own_stub, own_log) = own.spawn(&mut own.stub_command("silent"), "own-silent-stub.log");
    wait_stub_socket(&own, &mut own_stub, &own_log);
    let marker = format!("{LABEL}\n{}\n1\n", own_stub.0.id());
    std::fs::write(&own.marker, &marker).unwrap();
    let (status, log) = own.completed(
        &mut own.launch_command(&own.config),
        "own-silent-launch.log",
    );
    assert!(
        !status.success(),
        "unidentified own socket must refuse: {log}"
    );
    assert!(
        log.contains("preserving marker"),
        "wrong own-job refusal: {log}"
    );
    assert_eq!(std::fs::read_to_string(&own.marker).unwrap(), marker);
    assert!(
        own_stub.0.try_wait().unwrap().is_none(),
        "unidentified own stub was signalled"
    );
}

// MUST-FAIL: replacing the peer PID with the PID-file PID in the handover
// branch would signal the unrelated decoy, not the answering socket holder.
#[test]
fn supervisor_pid_file_decoy_never_selects_signal_target() {
    let fixture = Fixture::new();
    let mut unrelated = OwnedChild(Command::new("sleep").arg("60").spawn().unwrap());
    std::fs::write(&fixture.pid_file, format!("{}\n", unrelated.0.id())).unwrap();
    let (status, log) = fixture.completed(
        &mut fixture.launch_command(&fixture.config),
        "pid-only-launch.log",
    );
    assert!(
        !status.success(),
        "daemon should conservatively refuse live PID-file owner: {log}"
    );
    assert!(
        log.contains("live process owns the PID file"),
        "launcher did not reach daemon exec: {log}"
    );
    assert!(
        unrelated.0.try_wait().unwrap().is_none(),
        "unrelated PID-file process was signalled"
    );

    // Separate poison subcase: a probe-answering daemon-like stub owns the
    // socket while the PID file still names an unrelated live process.
    let poison = Fixture::new();
    let mut decoy = OwnedChild(Command::new("sleep").arg("60").spawn().unwrap());
    std::fs::write(&poison.pid_file, format!("{}\n", decoy.0.id())).unwrap();
    let (mut holder, holder_log) =
        poison.spawn(&mut poison.stub_command("answer"), "answer-stub.log");
    wait_stub_socket(&poison, &mut holder, &holder_log);
    let (status, log) = poison.completed(
        &mut poison.launch_command_options(&poison.config, &["kg"], "1", true),
        "poison-launch.log",
    );
    assert!(
        !status.success(),
        "runtime must refuse decoy PID-file owner: {log}"
    );
    assert!(
        log.contains("live process owns the PID file"),
        "holder was not handed over before daemon exec: {log}"
    );
    assert!(
        decoy.0.try_wait().unwrap().is_none(),
        "decoy PID-file process was signalled"
    );
    let deadline = Instant::now() + START_LIMIT;
    while holder.0.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "socket holder did not receive SIGTERM: {log}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

// MUST-FAIL: SIGTERM is bounded to one interval and never escalates to KILL.
#[tokio::test]
async fn supervisor_probe_answering_sigterm_ignoring_holder_stays_alive() {
    let fixture = Fixture::new();
    let (mut holder, holder_log) =
        fixture.spawn(&mut fixture.stub_command("ignore-term"), "ignore-stub.log");
    wait_stub_socket(&fixture, &mut holder, &holder_log);
    let start = Instant::now();
    let (status, log) = fixture.completed(
        &mut fixture.launch_command_options(&fixture.config, &["kg"], "1", true),
        "ignore-launch.log",
    );
    assert!(!status.success(), "ignoring holder must block exec: {log}");
    assert!(
        start.elapsed() >= Duration::from_secs(1),
        "launcher did not wait one interval: {log}"
    );
    assert!(
        log.contains("incumbent did not yield"),
        "wrong refusal: {log}"
    );
    assert!(
        log.contains(&holder.0.id().to_string()),
        "missing peer pid: {log}"
    );
    assert!(
        fixture.marker.exists(),
        "failed handover must retain marker"
    );
    assert!(holder.0.try_wait().unwrap().is_none(), "holder was killed");
    assert_eq!(socket_holder_pid(&fixture).await, Some(holder.0.id()));
}
