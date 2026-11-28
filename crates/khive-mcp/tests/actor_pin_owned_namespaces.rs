//! Actor grants follow the final identity, using the actor that declared them
//! as the baseline. These tests resolve configuration only; no store is opened.

use std::path::PathBuf;

use khive_mcp::serve::{resolve_runtime_config, RuntimeConfigInputs};
use khive_runtime::{Namespace, RuntimeConfig};

const CONFIGURED: &str = "lambda:configured";
const PROJECT: &str = "lambda:project";
const OWNED: &str = "lambda:owned";
const SHARED: &str = "lambda:shared";

// Each environment/cwd fixture is the sole test in a fresh process. In
// particular, serializing just this file would not isolate other test binaries.
fn run_in_isolated_child(test: &str) -> bool {
    const MARKER: &str = "KHIVE_MCP_ACTOR_OWNERSHIP_CHILD";
    if std::env::var(MARKER).as_deref() == Ok(test) {
        return false;
    }
    let root = tempfile::tempdir().expect("private child root");
    let home = root.path().join("home");
    std::fs::create_dir(&home).expect("private empty HOME");
    let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
    command.args(["--exact", test, "--nocapture"]);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("KHIVE_") {
            command.env_remove(key);
        }
    }
    let output = command
        .current_dir(root.path())
        .env("HOME", &home)
        .env_remove("LATTICE_MODEL_CACHE")
        .env(MARKER, test)
        .output()
        .expect("run exact isolated config test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("running 1 test") && stdout.contains("1 passed"),
        "isolated test must execute and pass exactly one test: {test}\n{stdout}\n{stderr}"
    );
    true
}

struct Fixture {
    previous_cwd: PathBuf,
    root: tempfile::TempDir,
    db: String,
}

impl Fixture {
    fn new() -> Self {
        let previous_cwd = std::env::current_dir().expect("previous cwd");
        let root = tempfile::tempdir().expect("private config fixture");
        let database = root.path().join("database/.khive");
        let project = root.path().join("project/.khive");
        std::fs::create_dir_all(&database).expect("database config directory");
        std::fs::create_dir_all(&project).expect("caller config directory");
        std::fs::write(
            database.join("config.toml"),
            format!(
                "[actor]\nid = \"{CONFIGURED}\"\nowned_namespaces = [\"{OWNED}\"]\nvisible_namespaces = [\"{SHARED}\"]\n"
            ),
        )
        .expect("configured actor and its grants");
        std::fs::write(
            project.join("config.toml"),
            format!("[actor]\nid = \"{PROJECT}\"\n"),
        )
        .expect("independent caller actor");
        std::env::set_current_dir(root.path().join("project")).expect("private caller cwd");
        let db = database.join("config-only.db").to_str().unwrap().to_owned();
        Self {
            previous_cwd,
            root,
            db,
        }
    }

    fn inputs(&self, namespace: &str, actor_explicit: bool) -> RuntimeConfigInputs<'_> {
        RuntimeConfigInputs {
            db: Some(&self.db),
            config: None,
            namespace: Namespace::parse(namespace).expect("fixture namespace"),
            namespace_explicit: actor_explicit,
            actor_explicit,
            no_embed: true,
            packs: Some(vec!["kg".to_owned()]),
            brain_profile: None,
        }
    }

    fn mcp(&self, actor: &str) -> RuntimeConfig {
        resolve_runtime_config(self.inputs(actor, true)).expect("MCP config resolution")
    }

    fn no_pin(&self) -> RuntimeConfig {
        resolve_runtime_config(self.inputs("local", false)).expect("MCP unpinned resolution")
    }

    fn set_project_actor(&self, actor: &str) {
        std::fs::write(
            self.root.path().join("project/.khive/config.toml"),
            format!("[actor]\nid = \"{actor}\"\n"),
        )
        .expect("replace caller actor");
    }

    fn qualify(&self) {
        let configured = self.mcp(CONFIGURED);
        assert_eq!(configured.actor_id.as_deref(), Some(CONFIGURED));
        assert_eq!(
            configured.owned_namespaces,
            vec![Namespace::parse(OWNED).unwrap()],
            "positive baseline: DB config must actually supply A's grant S"
        );
        let project = self.no_pin();
        assert_eq!(
            project.actor_id.as_deref(),
            Some(PROJECT),
            "positive baseline: an independent cwd config must select B"
        );
        assert!(
            !self
                .root
                .path()
                .join("database/.khive/config-only.db")
                .exists(),
            "configuration-only qualification must not open a store"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Runs before TempDir removes the directory containing the child cwd.
        std::env::set_current_dir(&self.previous_cwd).expect("restore child cwd");
    }
}

#[test]
fn mcp_explicit_configured_actor_retains_declared_grants() {
    if run_in_isolated_child("mcp_explicit_configured_actor_retains_declared_grants") {
        return;
    }
    let fixture = Fixture::new();
    fixture.qualify();
    let config = fixture.mcp(CONFIGURED);
    assert_eq!(config.actor_id.as_deref(), Some(CONFIGURED));
    assert_eq!(
        config.owned_namespaces,
        vec![Namespace::parse(OWNED).unwrap()],
        "MCP_CONFIGURED_ACTOR_RETAINS_DECLARED_GRANTS: final A keeps exactly S"
    );
    assert!(config
        .visible_namespaces
        .contains(&Namespace::parse(SHARED).unwrap()));
}

#[test]
fn mcp_explicit_other_actor_drops_declared_grants() {
    if run_in_isolated_child("mcp_explicit_other_actor_drops_declared_grants") {
        return;
    }
    let fixture = Fixture::new();
    fixture.qualify();
    let config = fixture.mcp("lambda:other");
    assert_eq!(config.actor_id.as_deref(), Some("lambda:other"));
    assert!(
        config.owned_namespaces.is_empty(),
        "C1_MCP_DIFFERENT_ACTOR_DROPS_GRANTS: C must carry none of A's grants"
    );
}

#[test]
fn mcp_explicit_anonymous_actor_drops_declared_grants() {
    if run_in_isolated_child("mcp_explicit_anonymous_actor_drops_declared_grants") {
        return;
    }
    let fixture = Fixture::new();
    fixture.qualify();
    let config = fixture.mcp("local");
    assert_eq!(config.actor_id, None);
    assert!(
        config.owned_namespaces.is_empty(),
        "MCP_ANONYMOUS_DROPS_GRANTS: anonymous must carry none of A's grants"
    );
}

#[test]
fn mcp_no_pin_configured_actor_keeps_declared_grants() {
    if run_in_isolated_child("mcp_no_pin_configured_actor_keeps_declared_grants") {
        return;
    }
    let fixture = Fixture::new();
    fixture.qualify();
    fixture.set_project_actor(CONFIGURED);
    let config = fixture.no_pin();
    assert_eq!(config.actor_id.as_deref(), Some(CONFIGURED));
    assert_eq!(
        config.owned_namespaces,
        vec![Namespace::parse(OWNED).unwrap()],
        "MCP_NO_PIN_RETAINS_DECLARED_GRANTS: unchanged declaring actor keeps S"
    );
    assert_eq!(config.default_namespace.as_str(), "local");
}

#[test]
fn mcp_project_actor_change_drops_declared_grants() {
    if run_in_isolated_child("mcp_project_actor_change_drops_declared_grants") {
        return;
    }
    let fixture = Fixture::new();
    fixture.qualify();
    let config = fixture.no_pin();
    assert_eq!(config.actor_id.as_deref(), Some(PROJECT));
    assert!(
        config.owned_namespaces.is_empty(),
        "MCP_PROJECT_CHANGE_DROPS_GRANTS: unpinned project B cannot inherit A's grants"
    );
    assert_eq!(config.default_namespace.as_str(), "local");
}
