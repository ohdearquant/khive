//! Tool profiles (ADR-181 Amendment 17) driven through the verb registry: a
//! registered tool the operator configured additions for gets them only on a
//! run a grant allowed for a named caller, and every other tool on the same
//! host runs under the unchanged sandbox.

use khive_pack_blob::BlobPack;
use khive_pack_exec::ExecPack;
use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::engine_config::{ExecSectionConfig, ExecToolProfileConfig};
use khive_runtime::{
    KhiveRuntime, RequestIdentity, RuntimeConfig, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::BlobStore;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

const OWNER: &str = "owner:ops";
const AGENT: &str = "agent:gpu";
const PROFILED: &str = "sh-lock";

struct Fixture {
    registry: VerbRegistry,
    _dir: tempfile::TempDir,
    root: PathBuf,
    /// The one file the profile for `PROFILED` names, outside the run root.
    lock: PathBuf,
}

fn identity(actor: &str) -> RequestIdentity {
    RequestIdentity {
        namespace: "local".into(),
        actor_id: Some(actor.into()),
        visible_namespaces: Vec::new(),
        owned_namespaces: Vec::new(),
        process_ref: None,
        request_id: None,
    }
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = dir.path().canonicalize().expect("canonical tempdir");
    let root = base.join("exec-root");
    let named = base.join("named");
    std::fs::create_dir(&named).expect("directory for the named file");
    let lock = named.join("gpu.lock");
    let cfg = RuntimeConfig {
        db_path: Some(base.join("khive.db")),
        exec: ExecSectionConfig {
            root: Some(root.to_string_lossy().to_string()),
            read_roots: vec!["/bin".into(), "/usr/bin".into()],
            timeout_default_s: Some(5.0),
            timeout_max_s: Some(10.0),
            tool_profiles: vec![ExecToolProfileConfig {
                tool: PROFILED.into(),
                files: vec![lock.to_string_lossy().to_string()],
                ..Default::default()
            }],
            ..Default::default()
        },
        ..RuntimeConfig::no_embeddings()
    };
    let rt = KhiveRuntime::new(cfg).expect("file runtime");
    let blobs =
        khive_db::stores::blob::FsBlobStore::new(base.join("blobs"), 0).expect("fs blob store");
    let store: Arc<dyn BlobStore> = Arc::new(blobs);
    rt.install_blob_store(store).expect("install blob store");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(BlobPack::new(rt.clone()));
    builder.register(ToolPack::new(rt.clone()));
    builder.register(ExecPack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    registry.apply_schema_plans(rt.backend());
    rt.install_edge_rules(registry.all_edge_rules());
    Fixture {
        registry,
        _dir: dir,
        root,
        lock,
    }
}

impl Fixture {
    async fn call_as(
        &self,
        actor: Option<&str>,
        verb: &str,
        params: Value,
    ) -> Result<Value, String> {
        self.registry
            .dispatch_with_identity(verb, params, actor.map(identity))
            .await
            .map_err(|error| error.to_string())
    }

    async fn ok_as(&self, actor: Option<&str>, verb: &str, params: Value) -> Value {
        self.call_as(actor, verb, params)
            .await
            .unwrap_or_else(|error| panic!("{verb} failed: {error}"))
    }

    async fn register(&self, name: &str) {
        self.ok_as(
            Some(OWNER),
            "tool.register",
            json!({
                "name": name, "kind": "tool", "description": "shell",
                "source": "exec:/bin/sh", "side_effect": "write", "trust": "first_party",
            }),
        )
        .await;
    }

    /// The agent asks for the tool and the owner grants it.
    async fn grant(&self, name: &str) {
        let request = self
            .ok_as(
                Some(AGENT),
                "tool.request",
                json!({ "tool": name, "actor": AGENT }),
            )
            .await;
        let granted = self
            .ok_as(
                Some(OWNER),
                "tool.grant",
                json!({ "id": request["request_id"] }),
            )
            .await;
        assert_eq!(granted["grant"]["status"], "granted", "{granted}");
    }

    /// Only the macOS run below materializes a tree.
    #[cfg(target_os = "macos")]
    async fn tree(&self) -> String {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"in");
        let put = self
            .ok_as(None, "blob.put", json!({ "bytes": encoded }))
            .await;
        let content_ref = put
            .get("ref")
            .or_else(|| put.get("content_ref"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("blob.put result has no ref: {put}"))
            .to_string();
        let tree = self
            .ok_as(
                None,
                "exec.tree",
                json!({ "entries": [{ "path": "input", "ref": content_ref, "mode": 644 }] }),
            )
            .await;
        tree["tree"].as_str().expect("tree ref").to_string()
    }
}

#[tokio::test]
async fn identity_reports_each_configured_tool_profile() {
    let f = fixture();
    let identity = f.ok_as(None, "exec.identity", json!({})).await;
    let profiles = identity["tool_profiles"]
        .as_array()
        .expect("tool_profiles list");
    assert_eq!(profiles.len(), 1, "{identity}");
    assert_eq!(profiles[0]["tool"], PROFILED);
    assert_eq!(profiles[0]["metal"], false);
    assert_eq!(profiles[0]["superseded_keys"], json!([]));
    assert_eq!(profiles[0]["files"], json!([f.lock.to_string_lossy()]));
    assert!(profiles[0]["rules_digest"]
        .as_str()
        .is_some_and(|d| d.len() == 64));
    assert_eq!(identity["system_root_links"], json!(["/etc", "/var"]));
    assert!(identity["system_read_roots"]
        .as_array()
        .expect("system_read_roots list")
        .contains(&json!("/bin")));
}

#[tokio::test]
async fn a_profiled_tool_allowed_by_policy_and_not_by_a_grant_is_refused() {
    let f = fixture();
    f.register(PROFILED).await;
    f.ok_as(
        Some(OWNER),
        "tool.policy",
        json!({ "actor": "*", "tool": PROFILED, "decision": "allow" }),
    )
    .await;
    let error = f
        .call_as(
            Some(AGENT),
            "exec.run",
            json!({ "tree": "not-read-before-the-refusal", "tool": PROFILED, "actor": AGENT }),
        )
        .await
        .expect_err("a policy allow does not earn the profile");
    assert!(error.contains("tool_profile_unhonoured:"), "{error}");
    assert!(error.contains("allowed by policy"), "{error}");
    assert!(
        !error.contains("tree:"),
        "refused before the tree is read: {error}"
    );
    assert!(!f.root.exists(), "a refusal materializes nothing");
    assert!(!f.lock.exists());
}

#[tokio::test]
async fn a_profiled_tool_is_refused_to_an_anonymous_caller_naming_a_granted_actor() {
    let f = fixture();
    f.register(PROFILED).await;
    f.grant(PROFILED).await;
    let error = f
        .call_as(
            None,
            "exec.run",
            json!({ "tree": "not-read-before-the-refusal", "tool": PROFILED, "actor": AGENT }),
        )
        .await
        .expect_err("an anonymous caller does not earn the profile");
    assert!(error.contains("tool_profile_unhonoured:"), "{error}");
    assert!(error.contains("anonymous"), "{error}");
    assert!(!f.root.exists(), "a refusal materializes nothing");
}

#[tokio::test]
async fn an_unprofiled_tool_on_the_same_host_is_not_refused_for_its_source() {
    // Control for the two refusals above: the same policy allow and the same
    // anonymous caller pass preflight's policy step for a tool with no
    // profile, and the run is refused later, for its tree.
    let f = fixture();
    f.register("sh").await;
    f.ok_as(
        Some(OWNER),
        "tool.policy",
        json!({ "actor": "*", "tool": "sh", "decision": "allow" }),
    )
    .await;
    let error = f
        .call_as(
            None,
            "exec.run",
            json!({ "tree": "intentionally-invalid-tree", "tool": "sh", "actor": AGENT }),
        )
        .await
        .expect_err("the tree is invalid");
    assert!(!error.contains("tool_profile_unhonoured"), "{error}");
    assert!(error.contains("tree:"), "{error}");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_granted_run_of_the_profiled_tool_reaches_the_named_file_and_no_other_tool_does() {
    let f = fixture();
    f.register(PROFILED).await;
    f.register("sh").await;
    f.grant(PROFILED).await;
    f.grant("sh").await;
    let tree = f.tree().await;
    let write = format!("printf held > {}", f.lock.display());
    let identity = f.ok_as(None, "exec.identity", json!({})).await;
    let rules_digest = identity["tool_profiles"][0]["rules_digest"].clone();
    assert!(
        rules_digest.as_str().is_some_and(|d| d.len() == 64),
        "{identity}"
    );

    // Control: the plain tool, granted the same way, cannot reach the file.
    let plain = f
        .ok_as(
            Some(AGENT),
            "exec.run",
            json!({ "tree": tree, "tool": "sh", "args": ["-c", write], "actor": AGENT }),
        )
        .await;
    let receipt = &plain["receipt"];
    assert_eq!(receipt["decision"]["source"], "grant", "{receipt}");
    assert_ne!(receipt["exit_code"], 0, "{receipt}");
    assert!(receipt["sandbox"]["tool_profile"].is_null(), "{receipt}");
    let plain_profile = receipt["sandbox"]["profile_digest"].clone();
    assert!(!f.lock.exists());

    let profiled = f
        .ok_as(
            Some(AGENT),
            "exec.run",
            json!({ "tree": tree, "tool": PROFILED, "args": ["-c", write], "actor": AGENT }),
        )
        .await;
    let receipt = &profiled["receipt"];
    assert_eq!(receipt["success"], true, "{receipt}");
    assert_eq!(receipt["decision"]["source"], "grant", "{receipt}");
    assert_eq!(receipt["changed"], json!([]), "{receipt}");
    assert_eq!(
        receipt["sandbox"]["tool_profile"]["tool"], PROFILED,
        "{receipt}"
    );
    assert_eq!(
        receipt["sandbox"]["tool_profile"]["files"],
        json!([f.lock.to_string_lossy()]),
        "{receipt}"
    );
    // The receipt names the additions by the digest identity reports, and the
    // profile it ran under is a different one from the plain tool's.
    assert_eq!(
        receipt["sandbox"]["tool_profile"]["rules_digest"], rules_digest,
        "{receipt}"
    );
    assert!(
        receipt["sandbox"]["profile_digest"].is_string(),
        "{receipt}"
    );
    assert_ne!(
        receipt["sandbox"]["profile_digest"], plain_profile,
        "{receipt}"
    );
    assert_eq!(
        std::fs::read(&f.lock).expect("the named file was written"),
        b"held"
    );

    // The profile names one file: its neighbour stays closed to the same run.
    let beside = f.lock.with_file_name("beside");
    let reach = format!("printf leaked > {}", beside.display());
    let refused = f
        .ok_as(
            Some(AGENT),
            "exec.run",
            json!({ "tree": tree, "tool": PROFILED, "args": ["-c", reach], "actor": AGENT }),
        )
        .await;
    assert_ne!(refused["receipt"]["exit_code"], 0, "{}", refused["receipt"]);
    assert!(!beside.exists());
}
