//! Real CLI dispatch and operator re-pin against the Rust stdio fixture.
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn fixture() -> PathBuf {
    let current = std::env::current_exe().unwrap();
    fs::read_dir(current.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("mount_fixture-")
                && (path.extension().is_none() || path.extension().is_some_and(|ext| ext == "exe"))
        })
        .max_by_key(|path| path.metadata().unwrap().modified().unwrap())
        .expect("Cargo must build mount_fixture alongside this test")
}
struct Fixture {
    root: tempfile::TempDir,
    config: PathBuf,
    state: PathBuf,
    database: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("khive.toml");
        let state = root.path().join("state");
        let database = root.path().join("main.db");
        let fixture = Self {
            root,
            config,
            state,
            database,
        };
        fixture.catalog(false);
        fixture.configure(false);
        fixture
    }
    fn catalog(&self, changed: bool) {
        fs::write(&self.state, json!({"tools": [{"name": "echo", "description": if changed { "new" } else { "old" }, "inputSchema": {"type": "object"}}]}).to_string()).unwrap();
    }
    fn configure(&self, denied: bool) {
        let gate = if denied {
            "[gate]\ngranted_actors = []\ngrant_unattributed = false\n"
        } else {
            ""
        };
        fs::write(&self.config, format!("[runtime]\npacks = [\"kg\", \"agent\"]\n[display]\ntimezone = \"UTC\"\n{gate}[[backends]]\nname = \"main\"\nkind = \"sqlite\"\npath = {:?}\n[packs.kg]\nbackend = \"main\"\nno_embed = true\n[packs.agent]\nbackend = \"main\"\nno_embed = true\n[[mounts]]\nname = \"demo\"\ntransport = \"stdio\"\ncommand = {:?}\nargs = [{:?}]\ntools = [{{ name = \"echo\", effect = \"read\" }}]\ntimeout_ms = 3000\n", self.database, fixture(), self.state)).unwrap();
    }
    fn run(&self, args: &[&str]) -> (Output, Value) {
        let output = Command::new(env!("CARGO_BIN_EXE_kkernel"))
            .args(args)
            .arg("--config")
            .arg(&self.config)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("KHIVE_NO_DAEMON", "1")
            .env("KHIVE_EVENTS_SPLIT", "0")
            .env("KHIVE_LOCK", self.root.path().join("boot.lock"))
            .env(
                "KHIVE_RECOVERER_LOCK",
                self.root.path().join("recover.lock"),
            )
            .env("KHIVE_MOUNTS_TEST_SECRET", "RAW_SECRET_SENTINEL")
            .current_dir(self.root.path())
            .output()
            .unwrap();
        let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "CLI JSON {error}; stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output, value)
    }
}
fn calls(path: &Path) -> usize {
    fs::read_to_string(format!("{}.calls", path.display()))
        .unwrap_or_default()
        .lines()
        .count()
}

#[test]
fn ordinary_exec_repin_and_agent_selection_use_the_served_registry() {
    let fixture = Fixture::new();
    let (output, body) = fixture.run(&["exec", "demo.echo(message=\"hello\")"]);
    assert!(output.status.success(), "{body}");
    assert_eq!(
        body["results"][0]["result"]["content"][0]["text"], "ok",
        "{body}"
    );
    assert_eq!(calls(&fixture.state), 1);
    fixture.catalog(true);
    let (_, body) = fixture.run(&["exec", "demo.echo()"]);
    assert_eq!(
        body["results"][0]["error"]["details"]["reason"], "catalog_drift",
        "{body}"
    );
    assert_eq!(calls(&fixture.state), 1);
    let (output, repin) = fixture.run(&["mount", "repin", "demo"]);
    assert!(output.status.success(), "{repin}");
    assert_eq!(repin["generation"], 2);
    assert_eq!(repin["changed"], json!(["echo"]));
    let (_, body) = fixture.run(&["exec", "demo.echo()"]);
    assert_eq!(body["results"][0]["ok"], true, "{body}");
    let (_, body) = fixture.run(&["exec", "verbs(pack=\"agent\")"]);
    assert_eq!(body["results"][0]["result"]["total"], 5, "{body}");
    let (_, body) = fixture.run(&["exec", "agent.spawn(provider=\"x\", task=\"t\")"]);
    assert_eq!(
        body["results"][0]["error"]["details"]["reason"], "provider_unavailable",
        "{body}"
    );
    fixture.configure(true);
    let before = calls(&fixture.state);
    let (_, body) = fixture.run(&["exec", "demo.echo()"]);
    assert_eq!(body["results"][0]["ok"], false, "{body}");
    assert_eq!(calls(&fixture.state), before);
}

#[test]
fn credential_reference_is_logged_by_name_and_inline_value_is_never_logged() {
    let fixture = Fixture::new();
    let original = fs::read_to_string(&fixture.config).unwrap();
    fs::write(
        &fixture.config,
        format!("{original}credential = \"KHIVE_MOUNTS_TEST_SECRET\"\n"),
    )
    .unwrap();
    let (output, body) = fixture.run(&["--log", "info", "exec", "demo.echo(mode=\"error\")"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("KHIVE_MOUNTS_TEST_SECRET"), "{stderr}");
    assert!(!stderr.contains("RAW_SECRET_SENTINEL"));
    assert!(!body.to_string().contains("RAW_SECRET_SENTINEL"));
    assert_eq!(
        body["results"][0]["error"]["details"]["class"], "tool_error",
        "{body}"
    );
    fs::write(
        &fixture.config,
        format!("{original}credential = \"sk-INLINE_SECRET_SENTINEL\"\n"),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_kkernel"))
        .args(["mount", "repin", "demo", "--config"])
        .arg(&fixture.config)
        .env_clear()
        .env("KHIVE_NO_DAEMON", "1")
        .current_dir(fixture.root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("credential"), "{stderr}");
    assert!(!stderr.contains("sk-INLINE_SECRET_SENTINEL"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("sk-INLINE_SECRET_SENTINEL"));
}
