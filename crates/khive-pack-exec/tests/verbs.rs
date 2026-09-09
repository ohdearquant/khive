//! Acceptance arms of ADR-181 driven through the verb registry against a
//! file-backed runtime with a real blob store. Runs that launch a process
//! use `/bin/sh` as the registered tool and only run on macOS, where
//! `sandbox-exec` exists.

use khive_pack_blob::BlobPack;
use khive_pack_exec::ExecPack;
use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::engine_config::ExecSectionConfig;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};

struct Fixture {
    registry: VerbRegistry,
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("exec-root");
    let db = dir.path().join("khive.db");
    let cfg = RuntimeConfig {
        db_path: Some(db),
        exec: ExecSectionConfig {
            root: Some(root.to_string_lossy().to_string()),
            read_roots: vec!["/bin".into(), "/usr/bin".into()],
            env: vec!["E1_ALLOWED".into()],
            never: vec!["/usr/bin/true".into()],
            max_output_bytes: Some(128),
            timeout_default_s: Some(5.0),
            timeout_max_s: Some(10.0),
            keep: false,
            limits: Default::default(),
        },
        // No embedding model: tool.register would otherwise build the default
        // embedder, which needs a model file the test host may not have.
        ..RuntimeConfig::no_embeddings()
    };
    let rt = KhiveRuntime::new(cfg).expect("file runtime");
    // A file-backed runtime installs no blob store on its own; the pack under
    // test materializes trees from blob refs, so give it a real one.
    let blobs = khive_db::stores::blob::FsBlobStore::new(dir.path().join("blobs"), 0)
        .expect("fs blob store");
    rt.install_blob_store(std::sync::Arc::new(blobs))
        .expect("install blob store");
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
    }
}

impl Fixture {
    async fn call(&self, verb: &str, params: Value) -> Value {
        self.registry
            .dispatch(verb, params)
            .await
            .unwrap_or_else(|e| panic!("{verb} failed: {e}"))
    }

    async fn call_err(&self, verb: &str, params: Value) -> String {
        match self.registry.dispatch(verb, params).await {
            Ok(v) => panic!("{verb} unexpectedly succeeded: {v}"),
            Err(e) => e.to_string(),
        }
    }

    async fn put(&self, bytes: &[u8]) -> String {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let v = self.call("blob.put", json!({ "bytes": encoded })).await;
        v.get("ref")
            .or_else(|| v.get("content_ref"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("blob.put result has no ref: {v}"))
            .to_string()
    }

    async fn tree(&self, files: &[(&str, &[u8], u32)]) -> String {
        let mut entries = Vec::new();
        for (path, bytes, mode) in files {
            let r = self.put(bytes).await;
            entries.push(json!({ "path": path, "ref": r, "mode": mode }));
        }
        let v = self.call("exec.tree", json!({ "entries": entries })).await;
        v["tree"].as_str().unwrap().to_string()
    }

    // Read only by the kernel-denial test below, which is macOS-only.
    #[cfg(target_os = "macos")]
    async fn blob_text(&self, r: &Value) -> String {
        use base64::Engine;
        let v = self.call("blob.get", json!({ "content_ref": r })).await;
        let b64 = v
            .get("bytes")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("blob.get result has no bytes: {v}"));
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .expect("blob bytes decode");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    async fn register_sh(&self, name: &str, decision: &str) {
        self.call(
            "tool.register",
            json!({
                "name": name,
                "kind": "tool",
                "description": "shell",
                "source": "exec:/bin/sh",
                "side_effect": "write",
                "trust": "first_party",
            }),
        )
        .await;
        self.call(
            "tool.policy",
            json!({ "actor": "*", "tool": name, "decision": decision }),
        )
        .await;
    }
}

fn root_is_empty(f: &Fixture) -> bool {
    match std::fs::read_dir(&f.root) {
        Ok(rd) => rd.count() == 0,
        Err(_) => true,
    }
}

#[tokio::test]
async fn tree_round_trip_and_diff() {
    let f = fixture();
    let base = f.tree(&[("a", b"1", 644), ("b", b"2", 755)]).await;
    let head = f
        .tree(&[("a", b"1", 644), ("b", b"3", 755), ("c", b"4", 644)])
        .await;
    let got = f.call("exec.tree_get", json!({ "tree": base })).await;
    assert_eq!(got["entries"].as_array().unwrap().len(), 2);
    let diff = f
        .call("exec.tree_diff", json!({ "base": base, "head": head }))
        .await;
    let ops: Vec<(String, String)> = diff["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["path"].as_str().unwrap().into(),
                c["op"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        ops,
        vec![
            ("b".into(), "modified".into()),
            ("c".into(), "added".into())
        ]
    );
    let same = f.tree(&[("b", b"2", 755), ("a", b"1", 644)]).await;
    assert_eq!(same, base, "manifest identity is order independent");
}

#[tokio::test]
async fn tree_refuses_unsafe_entries() {
    let f = fixture();
    let r = f.put(b"x").await;
    for bad in [
        json!([{ "path": "../escape", "ref": r, "mode": 644 }]),
        json!([{ "path": "/abs", "ref": r, "mode": 644 }]),
        json!([{ "path": "link", "ref": r, "mode": 0o120777 }]),
        json!([{ "path": "same", "ref": r, "mode": 644 }, { "path": "same", "ref": r, "mode": 644 }]),
        json!([{ "path": "m", "ref": r, "mode": 777 }]),
    ] {
        let err = f.call_err("exec.tree", json!({ "entries": bad })).await;
        assert!(!err.is_empty());
    }
}

#[tokio::test]
async fn refusals_write_receipts_and_touch_no_disk() {
    let f = fixture();
    let tree = f.tree(&[]).await;
    // Unregistered.
    let err = f
        .call_err(
            "exec.run",
            json!({ "tree": tree, "tool": "nope", "args": [], "actor": "local" }),
        )
        .await;
    assert!(err.contains("receipt_id="), "{err}");
    let id = err
        .split("receipt_id=")
        .nth(1)
        .unwrap()
        .trim_end_matches(')')
        .to_string();
    let receipt = f.call("exec.receipt", json!({ "id": id })).await;
    assert_eq!(receipt["denied"], true);
    assert!(receipt["exit_code"].is_null());
    assert!(receipt["tree_out"].is_null());
    assert!(root_is_empty(&f));
    // Deny policy carries the policy id.
    f.register_sh("sh-deny", "deny").await;
    let err = f
        .call_err(
            "exec.run",
            json!({ "tree": tree, "tool": "sh-deny", "args": ["-c", "true"], "actor": "local" }),
        )
        .await;
    let id = err
        .split("receipt_id=")
        .nth(1)
        .unwrap()
        .trim_end_matches(')')
        .to_string();
    let receipt = f.call("exec.receipt", json!({ "id": id })).await;
    assert_eq!(receipt["decision"]["decision"], "deny");
    assert_eq!(receipt["decision"]["source"], "policy");
    assert!(receipt["decision"]["id"].is_string());
    // Never binary refused even with allow.
    f.call(
        "tool.register",
        json!({ "name": "innocent", "kind": "tool", "description": "x", "source": "exec:/usr/bin/true", "side_effect": "read", "trust": "first_party" }),
    )
    .await;
    f.call(
        "tool.policy",
        json!({ "actor": "*", "tool": "innocent", "decision": "allow" }),
    )
    .await;
    let err = f
        .call_err(
            "exec.run",
            json!({ "tree": tree, "tool": "innocent", "args": [], "actor": "local" }),
        )
        .await;
    assert!(err.contains("never set"), "{err}");
    let events = f.call("exec.events", json!({})).await;
    assert_eq!(events["count"], 0, "refusals write no execution events");
    assert!(root_is_empty(&f));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn run_captures_output_changes_and_receipt() {
    let f = fixture();
    f.register_sh("sh", "allow").await;
    let tree = f
        .tree(&[
            ("modify", b"old", 644),
            ("remove", b"gone", 644),
            ("stay", b"same", 755),
        ])
        .await;
    let script = "printf new > modify; rm remove; printf added > added; chmod 755 added; echo out; echo err 1>&2; echo $HOME";
    let out = f
        .call(
            "exec.run",
            json!({
                "tree": tree, "tool": "sh", "args": ["-c", script], "actor": "local",
                "env": { "E1_ALLOWED": "caller", "DROP": "x" }, "session_id": "s1"
            }),
        )
        .await;
    let receipt = &out["receipt"];
    assert_eq!(receipt["denied"], false, "{receipt}");
    assert_eq!(receipt["exit_code"], 0, "{receipt}");
    assert_eq!(receipt["success"], true, "{receipt}");
    assert_eq!(receipt["seq"], 1);
    assert_eq!(receipt["argv"][0], "/bin/sh");
    let keys: Vec<&str> = receipt["env_keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["E1_ALLOWED", "HOME"]);
    let ops: std::collections::BTreeMap<String, String> = receipt["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["path"].as_str().unwrap().into(),
                c["op"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(ops.get("modify").map(String::as_str), Some("modified"));
    assert_eq!(ops.get("remove").map(String::as_str), Some("deleted"));
    assert_eq!(ops.get("added").map(String::as_str), Some("added"));
    assert_eq!(out["changed"], receipt["changed"]);
    let entries = f
        .call("exec.tree_get", json!({ "tree": receipt["tree_out"] }))
        .await;
    let names: Vec<&str> = entries["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["added", "modify", "stay"]);
    assert_eq!(receipt["stdout_capture"], "complete");
    assert!(receipt["stdout_produced_bytes"].as_u64().unwrap() > 0);
    assert_eq!(receipt["sandbox"]["profile_digest"], receipt["profile_ref"]);
    let stored = f.call("exec.receipt", json!({ "id": receipt["id"] })).await;
    assert_eq!(&stored, receipt, "wire receipt equals the durable row");
    let runs = f
        .call("exec.runs", json!({ "actor": "local", "session_id": "s1" }))
        .await;
    assert_eq!(runs["count"], 1);
    let events = f
        .call("exec.events", json!({ "run_id": receipt["id"] }))
        .await;
    let kinds: Vec<&str> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["materialized", "launched", "exited"]);
    assert!(root_is_empty(&f));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn run_refuses_cwd_escape_and_drops_undeclared_changes() {
    let f = fixture();
    f.register_sh("sh", "allow").await;
    let tree = f.tree(&[("keep", b"k", 644)]).await;
    let err = f
        .call_err(
            "exec.run",
            json!({ "tree": tree, "tool": "sh", "args": ["-c", "true"], "actor": "local", "cwd": "../out" }),
        )
        .await;
    assert!(err.contains("receipt_id="), "{err}");
    let out = f
        .call(
            "exec.run",
            json!({
                "tree": tree, "tool": "sh", "args": ["-c", "printf a > allowed; printf b > stray; rm keep"],
                "actor": "local", "declared_write_paths": ["allowed"]
            }),
        )
        .await;
    let receipt = &out["receipt"];
    assert_eq!(receipt["exit_code"], 0);
    assert_eq!(receipt["success"], false);
    let undeclared: Vec<&str> = receipt["undeclared_changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(undeclared, vec!["keep", "stray"]);
    let entries = f
        .call("exec.tree_get", json!({ "tree": receipt["tree_out"] }))
        .await;
    let names: Vec<&str> = entries["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["allowed", "keep"],
        "undeclared deletion keeps the input entry, undeclared addition is omitted"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn run_timeout_kills_the_group_and_output_tail_is_kept() {
    let f = fixture();
    f.register_sh("sh", "allow").await;
    let tree = f.tree(&[]).await;
    let out = f
        .call(
            "exec.run",
            json!({ "tree": tree, "tool": "sh", "args": ["-c", "sleep 5 & echo $!; sleep 5"], "actor": "local", "timeout_s": 0.5 }),
        )
        .await;
    let receipt = &out["receipt"];
    assert_eq!(receipt["timed_out"], true, "{receipt}");
    assert!(receipt["exit_code"].is_null());
    assert_eq!(receipt["success"], false);
    let big = f
        .call(
            "exec.run",
            json!({ "tree": tree, "tool": "sh", "args": ["-c", "head -c 512 /dev/zero | tr '\\0' A; printf OUT-END"], "actor": "local" }),
        )
        .await;
    let r = &big["receipt"];
    assert_eq!(r["stdout_produced_bytes"], 519);
    assert_eq!(r["stdout_retained_bytes"], 128);
    assert_eq!(r["stdout_capture"], "incomplete");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn run_declared_paths_cover_prefixes_at_slash_boundaries_only() {
    let f = fixture();
    f.register_sh("sh", "allow").await;
    let tree = f
        .tree(&[("a/x", b"x", 644), ("a.bak", b"bak", 644), ("b", b"b", 644)])
        .await;
    let out = f
        .call(
            "exec.run",
            json!({
                "tree": tree, "tool": "sh",
                "args": ["-c", "printf X > a/x; printf Y > a/y; printf BAK > a.bak; rm b"],
                "actor": "local", "declared_write_paths": ["a", "b"]
            }),
        )
        .await;
    let receipt = &out["receipt"];
    assert_eq!(receipt["exit_code"], 0, "{receipt}");
    assert_eq!(receipt["success"], false);
    let ops: Vec<(String, String)> = receipt["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["path"].as_str().unwrap().into(),
                c["op"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        ops,
        vec![
            ("a/x".to_string(), "modified".to_string()),
            ("a/y".to_string(), "added".to_string()),
            ("b".to_string(), "deleted".to_string()),
        ]
    );
    assert_eq!(receipt["undeclared_changes"], json!(["a.bak"]));
    let entries = f
        .call("exec.tree_get", json!({ "tree": receipt["tree_out"] }))
        .await;
    let bak = entries["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "a.bak")
        .expect("undeclared write keeps the input entry");
    assert_eq!(bak["ref"], json!(f.put(b"bak").await));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn concurrent_runs_in_one_session_get_distinct_seq() {
    let f = fixture();
    f.register_sh("sh", "allow").await;
    let tree = f.tree(&[]).await;
    let run = |i: u32| {
        f.call(
            "exec.run",
            json!({
                "tree": tree, "tool": "sh", "args": ["-c", format!("echo {i}")],
                "actor": "local", "session_id": "s-seq"
            }),
        )
    };
    let (a, b) = tokio::join!(run(1), run(2));
    let mut seqs = vec![
        a["receipt"]["seq"].as_i64().unwrap(),
        b["receipt"]["seq"].as_i64().unwrap(),
    ];
    seqs.sort_unstable();
    assert_eq!(seqs, vec![1, 2]);
    let runs = f
        .call(
            "exec.runs",
            json!({ "actor": "local", "session_id": "s-seq" }),
        )
        .await;
    assert_eq!(runs["count"], 2);
    let stored = f
        .call("exec.receipt", json!({ "id": a["receipt"]["id"] }))
        .await;
    assert_eq!(
        stored["seq"], a["receipt"]["seq"],
        "readers report the column"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn run_denies_version_control_and_the_never_set_at_the_kernel() {
    let f = fixture();
    f.register_sh("sh", "allow").await;
    let tree = f.tree(&[]).await;
    let control = f
        .call(
            "exec.run",
            json!({ "tree": tree, "tool": "sh", "args": ["-c", "/bin/echo ok"], "actor": "local" }),
        )
        .await;
    assert_eq!(control["receipt"]["exit_code"], 0, "{}", control["receipt"]);
    for cmd in ["git --version", "/usr/bin/true"] {
        let out = f
            .call(
                "exec.run",
                json!({ "tree": tree, "tool": "sh", "args": ["-c", cmd], "actor": "local" }),
            )
            .await;
        let receipt = &out["receipt"];
        assert_ne!(receipt["exit_code"], 0, "{cmd}: {receipt}");
        assert_eq!(receipt["success"], false, "{cmd}");
        let stderr = f.blob_text(&receipt["stderr_ref"]).await;
        assert!(
            stderr.contains("ermitted") || stderr.contains("ermission"),
            "{cmd}: the kernel refusal is in stderr: {stderr:?}"
        );
    }
}
