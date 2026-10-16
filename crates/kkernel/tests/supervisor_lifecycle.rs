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
const REAP_LIMIT: Duration = Duration::from_secs(2);

fn is_fixture_daemon_command(command: &str, config: &Path) -> bool {
    command.contains("kkernel mcp --daemon")
        && command.contains(&format!("--config {}", config.display()))
}

fn fixture_daemon_command(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-ww", "-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn fixture_daemon_alive(pid: u32, config: &Path) -> bool {
    fixture_daemon_command(pid).is_some_and(|command| is_fixture_daemon_command(&command, config))
}

fn socket_peer_pid(socket: &Path) -> Option<u32> {
    use std::os::fd::AsRawFd;

    let stream = std::os::unix::net::UnixStream::connect(socket).ok()?;
    let fd = stream.as_raw_fd();
    #[cfg(target_os = "linux")]
    {
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: fd is a connected fixture socket and cred is a live output buffer.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        (rc == 0 && len as usize == std::mem::size_of::<libc::ucred>())
            .then(|| u32::try_from(cred.pid).ok())
            .flatten()
    }
    #[cfg(target_os = "macos")]
    {
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: fd is a connected fixture socket and pid is a live output buffer.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut len,
            )
        };
        (rc == 0 && len as usize == std::mem::size_of::<libc::pid_t>())
            .then(|| u32::try_from(pid).ok())
            .flatten()
    }
}

fn fixture_daemon_pids(config: &Path) -> Vec<u32> {
    let Ok(output) = Command::new("ps")
        .args(["-ww", "-axo", "pid=,command="])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| is_fixture_daemon_command(line, config))
        .filter_map(|line| line.split_whitespace().next()?.parse::<u32>().ok())
        .collect()
}

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

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop_fixture_daemon();
    }
}

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
    fn stop_fixture_daemon(&self) {
        // A client can spawn a daemon that is not an OwnedChild. Prefer this
        // fixture's PID file, then check the peer of its own socket. Never
        // enumerate host processes or signal a PID whose command is not rooted
        // at this fixture's unique config path (including the PID-file decoy).
        let file_pid = std::fs::read_to_string(&self.pid_file)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .filter(|pid| fixture_daemon_alive(*pid, &self.config));
        let mut pids = Vec::new();
        if let Some(pid) = file_pid {
            pids.push(pid);
        }
        if let Some(pid) = socket_peer_pid(&self.socket) {
            if !pids.contains(&pid) && fixture_daemon_alive(pid, &self.config) {
                pids.push(pid);
            }
        }
        for pid in pids {
            let Ok(pid) = i32::try_from(pid) else {
                continue;
            };
            if pid <= 1 || !fixture_daemon_alive(pid as u32, &self.config) {
                continue;
            }
            // SAFETY: the PID was read from this fixture's PID file or socket
            // peer credentials, then revalidated against the fixture argv.
            let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
            let deadline = Instant::now() + REAP_LIMIT;
            while fixture_daemon_alive(pid as u32, &self.config) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if fixture_daemon_alive(pid as u32, &self.config) {
                // SAFETY: recheck the same fixture-specific argv immediately
                // before escalating; a reused PID cannot select a host daemon.
                let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
                let deadline = Instant::now() + REAP_LIMIT;
                while fixture_daemon_alive(pid as u32, &self.config) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

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
                "KHIVE_VOLUME_LOCK_DIR",
                self.root.path().join("volume-locks"),
            )
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

    fn locked_pid_command(&self) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "supervisor_locked_pid_child", "--nocapture"])
            .env("KHIVE_SUPERVISOR_STUB_PID_FILE", &self.pid_file)
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

fn assert_supervisor_marker(path: &Path, pid: u32, interval: u64) -> String {
    let marker = std::fs::read_to_string(path).unwrap();
    let mut lines = marker.lines();
    assert_eq!(lines.next(), Some(LABEL));
    assert_eq!(lines.next().unwrap().parse::<u32>().unwrap(), pid);
    assert_eq!(lines.next().unwrap().parse::<u64>().unwrap(), interval);
    let claim = lines.next().expect("marker incarnation claim");
    assert_eq!(uuid::Uuid::parse_str(claim).unwrap().to_string(), claim);
    assert_eq!(lines.next(), None);
    marker
}

include!("supervisor_lifecycle_tests/launcher_marker_tests.rs");

include!("supervisor_lifecycle_tests/client_handover_tests.rs");

include!("supervisor_lifecycle_tests/launch_order_and_ownership_tests.rs");

include!("supervisor_lifecycle_tests/handover_refusal_tests.rs");

include!("supervisor_lifecycle_tests/demand_retirement_tests.rs");

include!("supervisor_lifecycle_tests/private_socket_tests.rs");
