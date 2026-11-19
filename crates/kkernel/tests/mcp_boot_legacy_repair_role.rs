//! The MCP host records its role before it builds the server: a stdio host
//! that forwards to the daemon skips the legacy quarantine boot repair, while
//! the daemon and a daemonless host run it. Each role boots on a single store
//! and on a two-store configuration, which take different boot paths.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const REPAIR_LOG: &str = "legacy slugless quarantine original repair";

#[derive(Clone, Copy, Debug)]
enum Role {
    Forwarding,
    Daemonless,
    Daemon,
}

#[derive(Clone, Copy, Debug)]
enum Layout {
    SingleStore,
    TwoStores,
}

struct ProcessGroup(Child);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // SAFETY: the child leads its own process group (`process_group(0)`),
        // so this signals only that host and anything it spawned.
        unsafe {
            libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("KHIVE_") {
            command.env_remove(key);
        }
    }
    command
        .env_remove("LATTICE_MODEL_CACHE")
        .env("HOME", root.join("home"))
        .env("KHIVE_TEST_HARNESS", "1")
        .env("KHIVE_LOG", "info")
        .env("KHIVE_VOLUME_LOCK_DIR", root.join("volume-locks"))
        .env("KHIVE_WRITER_TIMEOUT_SINK_DIR", root.join("writer-logs"))
        .env("KHIVE_SOCKET", root.join("host.sock"))
        .env("KHIVE_PID", root.join("host.pid"))
        .env("KHIVE_LOCK", root.join("boot.lock"))
        .env("KHIVE_SQLITE_WAL_CEILING_BYTES", "0")
        .env("KHIVE_SQLITE_DISK_RESERVE_BYTES", "0")
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0);
    command
}

/// Boots `kkernel mcp` in `role` on scratch stores laid out as `layout` and
/// returns its boot log line about the repair.
fn repair_boot_line(role: Role, layout: Layout) -> String {
    let dir = tempfile::Builder::new()
        .prefix("lqr-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut command = command(&root);
    command.args(["mcp", "--no-embed", "--pack", "kg"]);
    match layout {
        Layout::SingleStore => {
            command.arg("--db").arg(root.join("main.db"));
        }
        Layout::TwoStores => {
            let config = root.join("khive.toml");
            let body = format!(
                "[runtime]\npacks = [\"kg\", \"session\"]\n\n\
                 [[backends]]\nname = \"main\"\nkind = \"sqlite\"\npath = {:?}\n\n\
                 [[backends]]\nname = \"sessions\"\nkind = \"sqlite\"\npath = {:?}\n\n\
                 [packs.session]\nbackend = \"sessions\"\nno_embed = true\n",
                root.join("main.db"),
                root.join("sessions.db"),
            );
            std::fs::write(&config, body).unwrap();
            command.args(["--pack", "session", "--config"]).arg(&config);
        }
    }
    match role {
        Role::Forwarding => {}
        Role::Daemonless => {
            command.env("KHIVE_NO_DAEMON", "1");
        }
        Role::Daemon => {
            command.arg("--daemon");
        }
    }
    let mut host = ProcessGroup(command.spawn().expect("spawn kkernel mcp"));
    let stderr = host.0.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if line.contains(REPAIR_LOG) {
                let _ = tx.send(Ok(line));
                return;
            }
            seen.push(line);
        }
        let _ = tx.send(Err(seen.join("\n")));
    });
    match rx.recv_timeout(Duration::from_secs(120)) {
        Ok(Ok(line)) => line,
        Ok(Err(stderr)) => {
            panic!("{role:?} host on {layout:?} exited without a repair boot line:\n{stderr}")
        }
        Err(error) => panic!("no repair boot line from the {role:?} host on {layout:?}: {error}"),
    }
}

fn assert_runs_the_repair(role: Role) {
    for layout in [Layout::SingleStore, Layout::TwoStores] {
        let line = repair_boot_line(role, layout);
        assert!(!line.contains("skipped"), "{role:?} on {layout:?}: {line}");
        assert!(line.contains("scans=1"), "{role:?} on {layout:?}: {line}");
    }
}

#[test]
fn a_forwarding_stdio_host_skips_the_legacy_quarantine_repair() {
    for layout in [Layout::SingleStore, Layout::TwoStores] {
        let line = repair_boot_line(Role::Forwarding, layout);
        assert!(line.contains("skipped"), "{layout:?}: {line}");
    }
}

#[test]
fn a_daemonless_stdio_host_runs_the_legacy_quarantine_repair() {
    assert_runs_the_repair(Role::Daemonless);
}

#[test]
fn the_daemon_runs_the_legacy_quarantine_repair() {
    assert_runs_the_repair(Role::Daemon);
}
