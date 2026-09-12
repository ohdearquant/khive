//! Keyed-memory CLI acceptance against real, independently owned daemons.
//!
//! Both packs use no-embed runtimes on one declared SQLite backend. The events
//! split is disabled: these tests do not cover events-daemon supervision or
//! ADR-133's post-commit obligation-failure response.

#![cfg(unix)]

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use khive_db::{ConnectionPool, PoolConfig};
use serde_json::{json, Value};
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(30);
const ACTOR: &str = "test:key-writer";

struct Process {
    child: Child,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl Process {
    fn finish(&mut self) -> (ExitStatus, Value) {
        let deadline = Instant::now() + WAIT;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("poll client") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "client timed out: {}",
                self.diagnostics()
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let bytes = fs::read(&self.stdout).expect("client stdout");
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("invalid client JSON: {error}; {}", self.diagnostics()));
        (status, body)
    }

    fn diagnostics(&self) -> String {
        format!(
            "pid={} stdout={} stderr={}",
            self.child.id(),
            fs::read_to_string(&self.stdout).unwrap_or_default(),
            fs::read_to_string(&self.stderr).unwrap_or_default()
        )
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        let _ = self.child.kill();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            if Instant::now() >= deadline {
                let message = format!("child was not reaped after kill: {}", self.diagnostics());
                if std::thread::panicking() {
                    eprintln!("{message}");
                    return;
                }
                panic!("{message}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

struct Fixture {
    // Processes must drop before their socket, database and log directory.
    daemons: Vec<Process>,
    root: TempDir,
    database: PathBuf,
    config: PathBuf,
    sequence: usize,
    completed: usize,
}

impl Fixture {
    fn new(daemon_count: usize) -> Self {
        assert!((1..=2).contains(&daemon_count));
        let root = tempfile::Builder::new()
            .prefix("memory-keys-")
            .tempdir_in("/tmp")
            .expect("short isolated socket directory");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir_all(root.path().join("home")).unwrap();
        fs::create_dir_all(root.path().join("tmp")).unwrap();
        let database = root.path().join("memory.db");
        let config = root.path().join("khive.toml");
        // exec has no --no-embed flag. The shared per-pack setting keeps its
        // engine fingerprint equal to the daemon's without embedding these notes.
        fs::write(
            &config,
            format!(
                "[runtime]\npacks = [\"kg\", \"memory\"]\n\
                 [display]\ntimezone = \"UTC\"\n\
                 [[backends]]\nname = \"main\"\nkind = \"sqlite\"\npath = {database:?}\n\
                 [packs.kg]\nbackend = \"main\"\nno_embed = true\n\
                 [packs.memory]\nbackend = \"main\"\nno_embed = true\n"
            ),
        )
        .unwrap();
        let mut fixture = Self {
            daemons: Vec::new(),
            root,
            database,
            config,
            sequence: 0,
            completed: 0,
        };
        for index in 0..daemon_count {
            let stdout = fixture.root.path().join(format!("daemon-{index}.stdout"));
            let stderr = fixture.root.path().join(format!("daemon-{index}.stderr"));
            let child = fixture
                .command(env!("CARGO_BIN_EXE_kkernel"), index)
                .args(["mcp", "--daemon", "--config"])
                .arg(&fixture.config)
                .args(["--pack", "kg", "--pack", "memory"])
                .stdin(Stdio::null())
                .stdout(File::create(&stdout).unwrap())
                .stderr(File::create(&stderr).unwrap())
                .spawn()
                .expect("spawn isolated daemon");
            fixture.daemons.push(Process {
                child,
                stdout,
                stderr,
            });
            let deadline = Instant::now() + WAIT;
            while !fixture.socket(index).exists() || !fixture.pid_file(index).exists() {
                let daemon = &mut fixture.daemons[index];
                assert!(
                    daemon.child.try_wait().unwrap().is_none(),
                    "daemon exited: {}",
                    daemon.diagnostics()
                );
                assert!(
                    Instant::now() < deadline,
                    "daemon did not bind: {}",
                    daemon.diagnostics()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            fixture.ok(index, ACTOR, operation("stats", json!({})));
        }
        if daemon_count == 2 {
            assert_ne!(fixture.daemons[0].child.id(), fixture.daemons[1].child.id());
            assert_ne!(fixture.socket(0), fixture.socket(1));
        }
        eprintln!("started {daemon_count} daemons on one scratch store; events split disabled");
        fixture
    }

    fn socket(&self, index: usize) -> PathBuf {
        self.root.path().join(format!("daemon-{index}.sock"))
    }

    fn pid_file(&self, index: usize) -> PathBuf {
        self.root.path().join(format!("daemon-{index}.pid"))
    }

    fn command(&self, executable: impl AsRef<std::ffi::OsStr>, index: usize) -> Command {
        let mut command = Command::new(executable);
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.root.path().join("home"))
            .env("USERPROFILE", self.root.path().join("home"))
            .env("TMPDIR", self.root.path().join("tmp"))
            .env("TZ", "UTC")
            .env("RUST_LOG", "error")
            .env("KHIVE_PACKS", "kg,memory")
            .env("KHIVE_EVENTS_SPLIT", "0")
            .env("KHIVE_DAEMON_STRICT", "1")
            .env("KHIVE_SOCKET", self.socket(index))
            .env("KHIVE_PID", self.pid_file(index))
            .env("KHIVE_LOCK", self.root.path().join("boot.lock"))
            .env(
                "KHIVE_RECOVERER_LOCK",
                self.root.path().join(format!("recover-{index}.lock")),
            )
            .current_dir(self.root.path());
        command
    }

    fn args(&self, actor: &str, op: Value) -> Vec<String> {
        vec![
            "exec".into(),
            json!([op]).to_string(),
            "--config".into(),
            self.config.to_str().unwrap().into(),
            "--actor".into(),
            actor.into(),
            "--presentation".into(),
            "verbose".into(),
            "--output-format".into(),
            "json".into(),
            "--strict".into(),
        ]
    }

    fn start(&mut self, index: usize, actor: &str, op: Value, gated: bool) -> (Process, PathBuf) {
        self.sequence += 1;
        let prefix = self.root.path().join(format!("client-{}", self.sequence));
        let stdout = prefix.with_extension("stdout");
        let stderr = prefix.with_extension("stderr");
        let ready = prefix.with_extension("ready");
        let args = self.args(actor, op);
        let mut command = if gated {
            let mut command = self.command(std::env::current_exe().unwrap(), index);
            command
                .args(["--ignored", "--exact", "memory_keys_client_process"])
                .env("KEYS_TEST_ARGS", serde_json::to_string(&args).unwrap())
                .env("KEYS_TEST_STDOUT", &stdout)
                .env("KEYS_TEST_READY", &ready)
                .stdin(Stdio::piped())
                .stdout(Stdio::null());
            command
        } else {
            let mut command = self.command(env!("CARGO_BIN_EXE_kkernel"), index);
            command
                .args(args)
                .stdin(Stdio::null())
                .stdout(File::create(&stdout).unwrap());
            command
        };
        let child = command
            .stderr(File::create(&stderr).unwrap())
            .spawn()
            .expect("spawn CLI client");
        (
            Process {
                child,
                stdout,
                stderr,
            },
            ready,
        )
    }

    fn receipt(&mut self, process: &mut Process) -> Value {
        let (status, body) = process.finish();
        let results = body["results"]
            .as_array()
            .unwrap_or_else(|| panic!("missing operation receipt: {body}"));
        assert_eq!(results.len(), 1, "one submitted operation: {body}");
        let result = results[0].clone();
        let ok = result["ok"].as_bool().expect("operation ok flag");
        assert_eq!(
            status.success(),
            ok,
            "strict CLI status disagrees: {body}; {}",
            process.diagnostics()
        );
        self.completed += 1;
        self.assert_daemons_owned();
        result
    }

    fn assert_daemons_owned(&mut self) {
        for index in 0..self.daemons.len() {
            let reported: u32 = fs::read_to_string(self.pid_file(index))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let daemon = &mut self.daemons[index];
            assert!(
                daemon.child.try_wait().unwrap().is_none(),
                "daemon stopped: {}",
                daemon.diagnostics()
            );
            assert_eq!(
                reported,
                daemon.child.id(),
                "CLI must not replace the fixture daemon"
            );
        }
    }

    fn call(&mut self, index: usize, actor: &str, op: Value) -> Value {
        let (mut process, _) = self.start(index, actor, op, false);
        self.receipt(&mut process)
    }

    fn ok(&mut self, index: usize, actor: &str, op: Value) -> Value {
        let receipt = self.call(index, actor, op);
        assert_eq!(receipt["ok"], true, "operation failed: {receipt}");
        receipt["result"].clone()
    }

    fn race(&mut self, first: Value, second: Value, actor: &str) -> [Value; 2] {
        assert_eq!(self.daemons.len(), 2, "race needs two distinct daemons");
        let (mut left, left_ready) = self.start(0, actor, first, true);
        let (mut right, right_ready) = self.start(1, actor, second, true);
        assert_ne!(left.child.id(), right.child.id(), "two OS clients required");
        let deadline = Instant::now() + WAIT;
        while !left_ready.exists() || !right_ready.exists() {
            assert!(
                left.child.try_wait().unwrap().is_none(),
                "left client exited: {}",
                left.diagnostics()
            );
            assert!(
                right.child.try_wait().unwrap().is_none(),
                "right client exited: {}",
                right.diagnostics()
            );
            assert!(
                Instant::now() < deadline,
                "clients did not reach the start barrier"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        eprintln!(
            "release clients {} and {} to daemons {} and {}",
            left.child.id(),
            right.child.id(),
            self.daemons[0].child.id(),
            self.daemons[1].child.id()
        );
        left.child.stdin.take().unwrap().write_all(&[1]).unwrap();
        right.child.stdin.take().unwrap().write_all(&[1]).unwrap();
        [self.receipt(&mut left), self.receipt(&mut right)]
    }

    fn memories(&mut self, namespace: &str) -> Vec<Value> {
        self.ok(
            0,
            ACTOR,
            operation(
                "list",
                json!({"kind": "memory", "namespace": namespace, "limit": 100}),
            ),
        )["items"]
            .as_array()
            .expect("memory list")
            .clone()
    }

    fn history_count(&self, namespace: &str, key: &str) -> i64 {
        // The acceptance race counts retained history, which public live-only lists omit.
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(self.database.clone()),
            ..PoolConfig::default()
        })
        .expect("live-WAL observer pool for this fixture's database");
        let reader = pool.reader().expect("scratch reader");
        reader
            .query_row(
                "SELECT count(*) FROM notes WHERE namespace = ?1 AND kind = 'memory' AND key = ?2",
                [namespace, key],
                |row| row.get(0),
            )
            .expect("count physical keyed history")
    }

    fn assert_executed(&self, minimum: usize) {
        assert!(
            self.completed >= minimum,
            "too few executed CLI receipts: {}",
            self.completed
        );
        eprintln!(
            "verified {} nonempty CLI operation receipts",
            self.completed
        );
    }
}

fn operation(tool: &str, args: Value) -> Value {
    json!({"tool": tool, "args": args})
}

fn remember(key: Option<&str>, namespace: Option<&str>, source: Option<&str>) -> Value {
    let mut args = json!({
        "content": "keyed memory acceptance record", "memory_type": "episodic",
        "salience": 0.25, "decay_factor": 0.0
    });
    for (field, value) in [
        ("key", key),
        ("namespace", namespace),
        ("source_id", source),
    ] {
        if let Some(value) = value {
            args[field] = json!(value);
        }
    }
    operation("memory.remember", args)
}

fn id(result: &Value) -> &str {
    let id = result["id"].as_str().expect("result UUID");
    uuid::Uuid::parse_str(id).expect("canonical full UUID");
    id
}

fn assert_conflict(receipt: &Value, key: &str, existing_id: &str) {
    assert_eq!(receipt["ok"], false, "replay must refuse: {receipt}");
    assert_eq!(receipt["error"]["kind"], "conflict", "{receipt}");
    assert_eq!(
        receipt["error"]["details"]["reason"], "key_conflict",
        "{receipt}"
    );
    assert_eq!(receipt["error"]["details"]["key"], key, "{receipt}");
    assert_eq!(
        receipt["error"]["details"]["existing_id"], existing_id,
        "{receipt}"
    );
    assert_eq!(
        receipt["error"]["domain_disposition"], "not_committed",
        "{receipt}"
    );
}

#[test]
#[ignore = "subprocess start-barrier helper, invoked by memory_keys concurrency tests"]
fn memory_keys_client_process() {
    let args: Vec<String> =
        serde_json::from_str(&std::env::var("KEYS_TEST_ARGS").expect("parent arguments")).unwrap();
    fs::write(
        std::env::var_os("KEYS_TEST_READY").expect("parent readiness path"),
        b"ready",
    )
    .unwrap();
    std::io::stdin()
        .read_exact(&mut [0])
        .expect("parent start barrier");
    let error = Command::new(env!("CARGO_BIN_EXE_kkernel"))
        .args(args)
        .stdout(
            File::create(std::env::var_os("KEYS_TEST_STDOUT").expect("parent output path"))
                .unwrap(),
        )
        .exec();
    panic!("exec fixture CLI: {error}");
}

#[test]
fn memory_keys_cli_replay_decoy_and_unkeyed_control() {
    let mut fixture = Fixture::new(1);
    let namespace = "keys:replay";
    let source = fixture.ok(
        0,
        ACTOR,
        operation(
            "create",
            json!({"kind": "observation", "content": "source observation", "namespace": namespace}),
        ),
    );
    let op = remember(Some("operation-a"), Some(namespace), Some(id(&source)));
    let first = fixture.ok(0, ACTOR, op.clone());
    // Discarding the success receipt from the consumer's perspective must not require a new identity.
    let replay = fixture.call(0, ACTOR, op.clone());
    let retained = fixture.history_count(namespace, "operation-a");
    eprintln!("first replay: retained_keyed_rows={retained}, receipt={replay}");
    assert_eq!(retained, 1, "replay must retain exactly one keyed holder");
    assert_conflict(&replay, "operation-a", id(&first));
    let mut changed_payload = op.clone();
    changed_payload["args"]["content"] = json!("a replay must not replace the holder's payload");
    assert_conflict(
        &fixture.call(0, ACTOR, changed_payload),
        "operation-a",
        id(&first),
    );
    assert_eq!(fixture.memories(namespace).len(), 1);
    let edges = fixture.ok(0, ACTOR, operation("list", json!({"kind": "edge", "namespace": namespace, "target_id": id(&source), "relations": ["annotates"]})));
    assert_eq!(edges["items"].as_array().unwrap().len(), 1);
    let decoy = fixture.ok(
        0,
        ACTOR,
        remember(Some("operation-b"), Some(namespace), Some(id(&source))),
    );
    assert_ne!(id(&first), id(&decoy));
    assert_conflict(&fixture.call(0, ACTOR, op), "operation-a", id(&first));
    assert_eq!(fixture.memories(namespace).len(), 2);
    let edges = fixture.ok(0, ACTOR, operation("list", json!({"kind": "edge", "namespace": namespace, "target_id": id(&source), "relations": ["annotates"]})));
    assert_eq!(edges["items"].as_array().unwrap().len(), 2);
    let unkeyed = remember(None, Some("keys:unkeyed"), Some(id(&source)));
    let one = fixture.ok(0, ACTOR, unkeyed.clone());
    let two = fixture.ok(0, ACTOR, unkeyed);
    assert_ne!(id(&one), id(&two));
    assert_eq!(fixture.memories("keys:unkeyed").len(), 2);
    let edges = fixture.ok(0, ACTOR, operation("list", json!({"kind": "edge", "namespace": "keys:unkeyed", "target_id": id(&source), "relations": ["annotates"]})));
    assert_eq!(edges["items"].as_array().unwrap().len(), 2);
    fixture.assert_executed(14);
}

#[test]
fn memory_keys_two_processes_two_daemons_share_one_holder() {
    let mut fixture = Fixture::new(2);
    const ROUNDS: usize = 4;
    for round in 0..ROUNDS {
        let namespace = format!("keys:concurrent:{round}");
        let op = remember(Some("same-operation"), Some(&namespace), None);
        let results = fixture.race(op.clone(), op, ACTOR);
        let successes: Vec<_> = results
            .iter()
            .filter(|result| result["ok"] == true)
            .collect();
        let notes = fixture.memories(&namespace);
        eprintln!(
            "concurrent round {round}: successes={}, live_rows={}",
            successes.len(),
            notes.len()
        );
        assert_eq!(successes.len(), 1, "exactly one create: {results:?}");
        let winner = id(&successes[0]["result"]);
        let loser = results.iter().find(|result| result["ok"] == false).unwrap();
        assert_conflict(loser, "same-operation", winner);
        assert_eq!(
            notes.len(),
            1,
            "partial unique index must reject the competing insert"
        );
        assert_eq!(id(&notes[0]), winner);
    }
    fixture.assert_executed(2 + 3 * ROUNDS);
}

#[test]
fn memory_keys_invalid_keys_leave_stats_unchanged() {
    let mut fixture = Fixture::new(1);
    let namespace = "keys:validation";
    let stats = operation("stats", json!({"namespace": namespace}));
    for key in [
        "x".repeat(513),
        "contains\0nul".into(),
        format!("{}x", "\u{00e9}".repeat(256)),
    ] {
        let before = fixture.ok(0, ACTOR, stats.clone());
        let invalid = fixture.call(0, ACTOR, remember(Some(&key), Some(namespace), None));
        assert_eq!(invalid["ok"], false, "invalid key must refuse: {invalid}");
        let error = invalid["error"].to_string().to_lowercase();
        assert!(
            error.contains("invalid") && error.contains("key"),
            "invalid-input key error required: {invalid}"
        );
        let after = fixture.ok(0, ACTOR, stats.clone());
        eprintln!(
            "invalid key bytes={}: before={before}, after={after}",
            key.len()
        );
        assert_eq!(before, after, "invalid key must not create notes or edges");
    }
    fixture.ok(
        0,
        ACTOR,
        remember(Some(&"x".repeat(512)), Some(namespace), None),
    );
    fixture.ok(
        0,
        ACTOR,
        remember(Some(&"\u{00e9}".repeat(256)), Some(namespace), None),
    );
    assert_eq!(
        fixture.memories(namespace).len(),
        2,
        "512 bytes, not 512 characters"
    );
    fixture.assert_executed(13);
}

#[test]
fn memory_keys_soft_and_hard_delete_release_the_key() {
    let mut fixture = Fixture::new(1);
    let namespace = "keys:release";
    let op = remember(Some("released-operation"), Some(namespace), None);
    let first = fixture.ok(0, ACTOR, op.clone());
    let prune = fixture.ok(
        0,
        ACTOR,
        operation(
            "memory.prune",
            json!({"namespace": namespace, "min_salience": 1.0, "before": 0}),
        ),
    );
    assert_eq!(prune["pruned"], 1);
    assert!(fixture.memories(namespace).is_empty());
    let second = fixture.ok(0, ACTOR, op.clone());
    assert_ne!(id(&first), id(&second));
    fixture.ok(
        0,
        ACTOR,
        operation("delete", json!({"id": id(&second), "hard": true})),
    );
    assert!(fixture.memories(namespace).is_empty());
    let third = fixture.ok(0, ACTOR, op);
    assert_ne!(id(&second), id(&third));
    assert_eq!(fixture.memories(namespace).len(), 1);
    fixture.assert_executed(9);
}

#[test]
fn memory_keys_actor_scope_and_explicit_namespace_pin() {
    let mut fixture = Fixture::new(2);
    let other = "test:recovering-writer";
    let op = remember(Some("unpinned-operation"), None, None);
    let first = fixture.ok(0, ACTOR, op.clone());
    assert_conflict(
        &fixture.call(0, ACTOR, op.clone()),
        "unpinned-operation",
        id(&first),
    );
    let changed_actor = fixture.ok(1, other, op);
    assert_ne!(
        id(&first),
        id(&changed_actor),
        "a different namespace is a different identity"
    );
    assert_eq!(fixture.memories(ACTOR).len(), 1);
    assert_eq!(fixture.memories(other).len(), 1);
    let pinned = remember(Some("pinned-operation"), Some(ACTOR), None);
    let original = fixture.ok(0, ACTOR, pinned.clone());
    assert_conflict(
        &fixture.call(1, other, pinned),
        "pinned-operation",
        id(&original),
    );
    assert_eq!(fixture.memories(ACTOR).len(), 2);
    assert_eq!(fixture.memories(other).len(), 1);
    fixture.assert_executed(11);
}

#[test]
fn memory_keys_prune_replay_race_bounds_retained_history() {
    let mut fixture = Fixture::new(2);
    const ROUNDS: usize = 12;
    let mut outcomes = [0usize; 2];
    for round in 0..ROUNDS {
        let namespace = format!("keys:prune-race:{round}");
        let key = "pruning-operation";
        let op = remember(Some(key), Some(&namespace), None);
        let original = fixture.ok(0, ACTOR, op.clone());
        let before = fixture.history_count(&namespace, key);
        assert_eq!(before, 1);
        let results = fixture.race(
            op,
            operation(
                "memory.prune",
                json!({"namespace": namespace, "min_salience": 1.0, "before": 0}),
            ),
            ACTOR,
        );
        assert_eq!(results[1]["ok"], true, "prune failed: {:?}", results[1]);
        assert_eq!(results[1]["result"]["pruned"], 1);
        let after = fixture.history_count(&namespace, key);
        let live = fixture.memories(&namespace).len();
        eprintln!(
            "prune race {round}: before_history={before}, after_history={after}, live_rows={live}"
        );
        assert!(
            (1..=2).contains(&after),
            "one original plus at most one replay, never a third"
        );
        if results[0]["ok"] == true {
            assert_ne!(id(&results[0]["result"]), id(&original));
            assert_eq!((after, live), (2, 1));
        } else {
            assert_conflict(&results[0], key, id(&original));
            assert_eq!((after, live), (1, 0));
        }
        outcomes[(after - 1) as usize] += 1;
    }
    assert_eq!(
        outcomes.iter().sum::<usize>(),
        ROUNDS,
        "every race must execute"
    );
    eprintln!(
        "prune race historical outcomes: one_row={}, two_rows={}",
        outcomes[0], outcomes[1]
    );
    fixture.assert_executed(2 + 4 * ROUNDS);
}

#[test]
#[ignore = "requires KEYS_TEST_PYTHON pointing to an existing interpreter with the Python client dependencies"]
fn memory_keys_native_python_conflict_matches_cli() {
    let python = std::env::var_os("KEYS_TEST_PYTHON")
        .expect("set KEYS_TEST_PYTHON to an existing interpreter; this fixture installs nothing");
    let mut fixture = Fixture::new(1);
    let namespace = "keys:python-parity";
    let key = "python-operation";
    let source = fixture.ok(
        0,
        ACTOR,
        operation(
            "create",
            json!({
                "kind": "observation", "content": "Python parity source", "namespace": namespace
            }),
        ),
    );
    let script = r#"
import json
import sys
from khive import Session, SocketTransport

class CountingTransport:
    def __init__(self, socket):
        self.inner = SocketTransport(socket)
        self.memory_calls = 0

    def round_trip(self, frame, timeout):
        if frame.get("ops"):
            self.memory_calls += 1
        return self.inner.round_trip(frame, timeout)

socket, actor, namespace, key, source_id = sys.argv[1:]
transport = CountingTransport(socket)
session = Session(transport, actor_id=actor, timeout=10.0)
args = dict(key=key, namespace=namespace, source_id=source_id,
            memory_type="episodic", salience=0.25, decay_factor=0.0)
first = session.remember("keyed memory acceptance record", **args)
assert first["ok"], first
replay = session.remember("keyed memory acceptance record", **args)
assert not replay["ok"], replay
assert replay["error"]["details"]["existing_id"] == first["result"]["id"], replay
assert transport.memory_calls == 2, transport.memory_calls
print(json.dumps(dict(first=first, replay=replay, memory_calls=transport.memory_calls)))
"#;
    let stdout = fixture.root.path().join("python.stdout");
    let stderr = fixture.root.path().join("python.stderr");
    let child = fixture
        .command(python, 0)
        .env(
            "PYTHONPATH",
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../python"),
        )
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .args(["-c", script])
        .arg(fixture.socket(0))
        .args([ACTOR, namespace, key, id(&source)])
        .stdin(Stdio::null())
        .stdout(File::create(&stdout).unwrap())
        .stderr(File::create(&stderr).unwrap())
        .spawn()
        .expect("spawn explicitly supplied Python interpreter");
    let mut process = Process {
        child,
        stdout,
        stderr,
    };
    let (status, python_result) = process.finish();
    assert!(
        status.success(),
        "native Python failed: {}",
        process.diagnostics()
    );
    fixture.assert_daemons_owned();
    assert_eq!(
        python_result["memory_calls"], 2,
        "reconciliation must stop at the holder, without a third memory call"
    );
    let holder = id(&python_result["first"]["result"]);
    assert_conflict(&python_result["replay"], key, holder);
    eprintln!("native Python recovery stopped after two memory calls: {python_result}");
    // This independent CLI invocation checks surface parity, not recovery retry policy.
    let cli = fixture.call(
        0,
        ACTOR,
        remember(Some(key), Some(namespace), Some(id(&source))),
    );
    assert_conflict(&cli, key, holder);
    assert_eq!(
        python_result["replay"]["error"], cli["error"],
        "Python must retain the entire CLI conflict object"
    );
    assert_eq!(fixture.memories(namespace).len(), 1);
    fixture.assert_executed(4);
}
