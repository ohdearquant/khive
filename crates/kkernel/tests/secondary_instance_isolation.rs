use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use khive_db::StorageBackend;
use serde_json::{json, Value};
use tempfile::TempDir;
use uuid::Uuid;

struct Fixture {
    _root: TempDir,
    home: PathBuf,
    cwd: PathBuf,
    sandbox: PathBuf,
    config: PathBuf,
    original_id: Uuid,
    last_child: Cell<(&'static str, Option<u32>)>,
}

const DIAGNOSTIC_CONTENT_LIMIT: u64 = 16 * 1024;

fn child_diagnostic_path(pid: u32) -> PathBuf {
    PathBuf::from(format!(".khive/logs/writer_timeouts.{pid}.ndjson"))
}

fn print_diagnostic(path: &Path, phase: &str) {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            let _ = writeln!(
                std::io::stderr().lock(),
                "{phase}: diagnostic {} cannot be opened: {error}",
                path.display(),
            );
            return;
        }
    };
    let size = file
        .metadata()
        .map(|metadata| metadata.len().to_string())
        .unwrap_or_else(|error| format!("unknown ({error})"));
    let mut content = Vec::new();
    let read = file
        .take(DIAGNOSTIC_CONTENT_LIMIT + 1)
        .read_to_end(&mut content);
    let truncated = content.len() as u64 > DIAGNOSTIC_CONTENT_LIMIT;
    content.truncate(DIAGNOSTIC_CONTENT_LIMIT as usize);
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(
        stderr,
        "{phase}: diagnostic {} ({size} bytes):\n{}",
        path.display(),
        String::from_utf8_lossy(&content),
    );
    if truncated {
        let _ = writeln!(stderr, "[truncated after {DIAGNOSTIC_CONTENT_LIMIT} bytes]");
    }
    if let Err(error) = read {
        let _ = writeln!(stderr, "[diagnostic read failed: {error}]");
    }
}

fn config_for(database: &Path) -> String {
    format!(
        r#"[runtime]
packs = ["kg"]
[actor]
id = "secondary-instance-fixture"
[[backends]]
name = "main"
kind = "sqlite"
path = {}
[packs.kg]
backend = "main"
no_embed = true
"#,
        serde_json::to_string(database.to_str().expect("UTF-8 fixture path")).unwrap(),
    )
}

fn successful_json(output: Output) -> Value {
    assert!(
        output.status.success(),
        "CLI failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_slice(&output.stdout).expect("JSON CLI response")
}

fn result_id(response: &Value) -> Uuid {
    assert_eq!(response["results"][0]["ok"], true, "{response}");
    Uuid::parse_str(
        response["results"][0]["result"]["id"]
            .as_str()
            .expect("created entity UUID"),
    )
    .expect("full entity UUID")
}

fn snapshot_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, directory: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(directory).expect("read fixture directory") {
            let entry = entry.expect("fixture directory entry");
            let path = entry.path();
            if entry.file_type().expect("fixture file type").is_dir() {
                visit(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(&path).expect("read fixture file"),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

fn assert_files_unchanged(
    root: &Path,
    before: &BTreeMap<PathBuf, Vec<u8>>,
    phase: &str,
    diagnostic_pid: Option<u32>,
) -> BTreeMap<PathBuf, Vec<u8>> {
    let after = snapshot_files(root);
    let diagnostic = diagnostic_pid.map(child_diagnostic_path);
    let mut changes = Vec::new();
    let allowed_added = diagnostic.as_ref().filter(|path| {
        if before.contains_key(*path) {
            return false;
        }
        match std::fs::symlink_metadata(root.join(path)) {
            Ok(metadata) if metadata.file_type().is_file() => true,
            Ok(_) => {
                changes.push(format!(
                    "added {} is not a regular non-symlink diagnostic file",
                    path.display(),
                ));
                false
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                changes.push(format!(
                    "cannot inspect diagnostic {}: {error}",
                    path.display(),
                ));
                false
            }
        }
    });
    let paths: BTreeSet<_> = before.keys().chain(after.keys()).collect();
    changes.extend(paths.into_iter().filter_map(|path| {
        match (before.get(path), after.get(path)) {
            (None, Some(_)) if allowed_added == Some(path) => None,
            (None, Some(bytes)) => {
                Some(format!("added {} ({} bytes)", path.display(), bytes.len()))
            }
            (Some(bytes), None) => Some(format!(
                "removed {} ({} bytes)",
                path.display(),
                bytes.len()
            )),
            (Some(old), Some(new)) if old != new => Some(format!(
                "changed {} ({} -> {} bytes)",
                path.display(),
                old.len(),
                new.len()
            )),
            _ => None,
        }
    }));
    assert!(
        changes.is_empty(),
        "{phase}: files under {} changed:\n{}",
        root.display(),
        changes.join("\n"),
    );
    after
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("private instance fixture");
        let home = root.path().join("home");
        let cwd = root.path().join("project");
        let sandbox = root.path().join("sandbox");
        for directory in [home.join(".khive"), cwd.join(".khive"), sandbox.clone()] {
            std::fs::create_dir_all(directory).expect("create fixture directory");
        }
        let original_db = home.join(".khive/khive.db");
        let original_config = config_for(&original_db);
        for path in [
            home.join(".khive/config.toml"),
            cwd.join("khive.toml"),
            cwd.join(".khive/config.toml"),
        ] {
            std::fs::write(path, &original_config).expect("write competing config");
        }
        let config = sandbox.join("config.toml");
        std::fs::write(&config, config_for(&sandbox.join("khive.db")))
            .expect("write selected sandbox config");
        let mut fixture = Self {
            _root: root,
            home,
            cwd,
            sandbox,
            config,
            original_id: Uuid::nil(),
            last_child: Cell::new(("fixture setup", None)),
        };
        let (_, output) = fixture.run(
            fixture
                .command()
                .env("KHIVE_CONFIG", fixture.home.join(".khive/config.toml"))
                .arg(r#"create(kind="concept", name="original instance sentinel")"#),
            "seed private original instance",
        );
        let seeded = successful_json(output);
        fixture.original_id = result_id(&seeded);
        // Preserve any committed WAL left by the exited child along with its database.
        for suffix in ["", "-wal", "-shm"] {
            let filename = format!("khive.db{suffix}");
            let source = fixture.home.join(".khive").join(&filename);
            if source.exists() {
                std::fs::copy(source, fixture.sandbox.join(filename))
                    .expect("copy closed original database into sandbox");
            }
        }
        fixture
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
        command
            .env_clear()
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("KHIVE_CONFIG", &self.config)
            .env("KHIVE_NO_DAEMON", "1")
            .env("KHIVE_LOCK", self.sandbox.join("boot.lock"))
            .env("KHIVE_RECOVERER_LOCK", self.sandbox.join("recoverer.lock"))
            .arg("exec")
            .args(["--strict", "--output-format", "json"]);
        command
    }

    fn run(&self, command: &mut Command, phase: &'static str) -> (u32, Output) {
        self.last_child.set((phase, None));
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("{phase}: spawn isolated exec: {error}"));
        let pid = child.id();
        self.last_child.set((phase, Some(pid)));
        let output = child
            .wait_with_output()
            .unwrap_or_else(|error| panic!("{phase}: wait for child {pid}: {error}"));
        // The sink writes startup independently of timeout events. Preserve
        // its HOME contract and expose the actual records even on success.
        print_diagnostic(&self.home.join(child_diagnostic_path(pid)), phase);
        (pid, output)
    }

    fn inline(&self, ops: &str, phase: &'static str) -> (u32, Value) {
        let (pid, output) = self.run(self.command().arg(ops), phase);
        (pid, successful_json(output))
    }

    fn print_failure_diagnostics(&self) {
        let (phase, pid) = self.last_child.get();
        let context = match pid {
            Some(pid) => format!("failure after {phase} (child {pid})"),
            None => format!("failure during {phase} before child spawn"),
        };
        let logs = self.home.join(".khive/logs");
        let entries = match std::fs::read_dir(&logs) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "{context}: cannot list diagnostics {}: {error}",
                    logs.display(),
                );
                return;
            }
        };
        for entry in entries {
            match entry {
                Ok(entry) if entry.path().extension().is_some_and(|ext| ext == "ndjson") => {
                    print_diagnostic(&entry.path(), &context);
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "{context}: cannot read diagnostic entry: {error}",
                    );
                }
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Dump before TempDir removes the evidence, including failures
            // before a snapshot assertion. Diagnostics must not double-panic.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.print_failure_diagnostics();
            }));
        }
    }
}

#[tokio::test]
async fn explicit_config_daemonless_data_writes_stay_in_the_selected_instance() {
    let fixture = Fixture::new();
    let mut protected = snapshot_files(&fixture.home);

    let (pid, existing) = fixture.inline(
        &format!(r#"get(id="{}")"#, fixture.original_id),
        "sandbox sentinel read",
    );
    assert_eq!(result_id(&existing), fixture.original_id);
    protected = assert_files_unchanged(
        &fixture.home,
        &protected,
        "sandbox sentinel read",
        Some(pid),
    );

    let (pid, created) = fixture.inline(
        r#"create(kind="concept", name="sandbox inline sentinel")"#,
        "sandbox inline create",
    );
    let inline_id = result_id(&created);
    protected = assert_files_unchanged(
        &fixture.home,
        &protected,
        "sandbox inline create",
        Some(pid),
    );
    let (pid, retrieved) = fixture.inline(
        &format!(r#"get(id="{inline_id}")"#),
        "sandbox inline readback",
    );
    assert_eq!(result_id(&retrieved), inline_id);
    protected = assert_files_unchanged(
        &fixture.home,
        &protected,
        "sandbox inline readback",
        Some(pid),
    );

    let input = fixture.sandbox.join("ops.jsonl");
    std::fs::write(
        &input,
        json!({"tool":"create", "args":{"kind":"concept", "name":"sandbox bulk sentinel"}})
            .to_string(),
    )
    .expect("write bulk operation");
    let output_file = fixture.sandbox.join("receipts.jsonl");
    let (pid, output) = fixture.run(
        fixture
            .command()
            .arg("--ops-file")
            .arg(input)
            .arg("--save-file")
            .arg(&output_file),
        "sandbox bulk create",
    );
    successful_json(output);
    let receipt: Value = serde_json::from_str(
        std::fs::read_to_string(output_file)
            .expect("read bulk receipt")
            .trim(),
    )
    .expect("one JSONL operation receipt");
    let bulk_id = result_id(&json!({"results":[receipt]}));

    assert_files_unchanged(&fixture.home, &protected, "sandbox bulk create", Some(pid));
    assert!(
        fixture.sandbox.join("khive.db.events.db").exists(),
        "audit persistence must be anchored beside the selected main backend",
    );

    for (database, expected) in [
        (fixture.sandbox.join("khive.db"), true),
        (fixture.home.join(".khive/khive.db"), false),
    ] {
        let backend =
            StorageBackend::sqlite_read_only(&database).expect("open private DB read-only");
        let entities = backend
            .entities_for_namespace("local")
            .expect("entity store");
        assert!(entities
            .get_entity(fixture.original_id)
            .await
            .unwrap()
            .is_some());
        for id in [inline_id, bulk_id] {
            assert_eq!(
                entities.get_entity(id).await.unwrap().is_some(),
                expected,
                "persisted entity {id} in {}",
                database.display(),
            );
        }
    }
}

#[test]
fn missing_explicit_config_refuses_before_writing_either_instance() {
    let fixture = Fixture::new();
    let before_home = snapshot_files(&fixture.home);
    let before_sandbox = snapshot_files(&fixture.sandbox);
    let (_, output) = fixture.run(
        fixture
            .command()
            .env("KHIVE_CONFIG", fixture.sandbox.join("missing.toml"))
            .arg(r#"create(kind="concept", name="must never be created")"#),
        "missing explicit config",
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing.toml"));
    assert_files_unchanged(
        &fixture.home,
        &before_home,
        "missing explicit config / original instance",
        None,
    );
    assert_files_unchanged(
        &fixture.sandbox,
        &before_sandbox,
        "missing explicit config / sandbox instance",
        None,
    );
}
