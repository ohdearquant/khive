//! Real launcher/daemon process controls for ADR-185 Amendment 1.
//! All configuration, rendezvous files, logs and children belong to this fixture.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs::File;
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
        let mut command = self.command();
        command
            .args([
                "supervisor",
                "launch",
                "--label",
                LABEL,
                "--restart-interval-secs",
                "10",
                "--",
                "--config",
            ])
            .arg(config)
            .args(["--no-embed", "--pack", "kg", "--actor", LABEL]);
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
