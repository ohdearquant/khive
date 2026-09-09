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
    blobs: std::path::PathBuf,
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
    let blob_root = dir.path().join("blobs");
    let blobs =
        khive_db::stores::blob::FsBlobStore::new(blob_root.clone(), 0).expect("fs blob store");
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
        blobs: blob_root,
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

/// Count the objects the blob store holds. `exec.tree_put` promises that a refused call leaves
/// no new object behind, and that is a claim about the store, not about the call's return value,
/// so the arm asserting it has to look at the store itself.
fn blob_object_count(f: &Fixture) -> usize {
    fn walk(dir: &std::path::Path, seen: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => walk(&entry.path(), seen),
                Ok(_) => *seen += 1,
                Err(_) => {}
            }
        }
    }
    let mut seen = 0;
    walk(&f.blobs, &mut seen);
    seen
}

async fn entries_of(f: &Fixture, tree: &str) -> Vec<(String, String, u64)> {
    f.call("exec.tree_get", json!({ "tree": tree })).await["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["path"].as_str().unwrap().to_string(),
                e["ref"].as_str().unwrap().to_string(),
                e["mode"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn tree_put_applies_edits_and_leaves_the_base_tree_alone() {
    let f = fixture();
    let base = f
        .tree(&[("a.txt", b"one", 644), ("keep/b.txt", b"two", 755)])
        .await;
    let replacement = f.put(b"three").await;
    let out = f
        .call(
            "exec.tree_put",
            json!({ "tree": base, "edits": [
                { "path": "a.txt", "content": "ONE" },
                { "path": "keep/b.txt", "ref": replacement },
                { "path": "new/c.txt", "content": "four", "mode": 755 },
                // A new path with no mode is the only edit that reaches the default, so it is the
                // only one that can tell 644 from the octal literal 0o644, which is 420.
                { "path": "new/d.txt", "content": "five" },
            ]}),
        )
        .await;
    let head = out["tree"].as_str().unwrap();
    assert_ne!(head, base, "a put mints a new tree");
    assert_eq!(out["base"], json!(base));
    assert_eq!(out["entries"], json!(4));

    let head_entries = entries_of(&f, head).await;
    assert_eq!(
        head_entries
            .iter()
            .map(|(path, _, mode)| (path.as_str(), *mode))
            .collect::<Vec<_>>(),
        vec![
            ("a.txt", 644),
            ("keep/b.txt", 755),
            ("new/c.txt", 755),
            ("new/d.txt", 644)
        ],
        "an edit without a mode keeps the mode the entry already had, and a new entry takes the \
         mode it was given"
    );
    assert_eq!(
        head_entries[1].1, replacement,
        "a ref edit stores the ref it was handed"
    );

    // Trees are immutable: reading the base back must show the pre-edit content.
    let base_entries = entries_of(&f, &base).await;
    assert_eq!(base_entries.len(), 2);
    assert_ne!(
        base_entries[0].1, head_entries[0].1,
        "the base still names the old content"
    );
    assert_eq!(
        f.call("exec.tree_diff", json!({ "base": base, "head": head }))
            .await["changed"]
            .as_array()
            .unwrap()
            .len(),
        4
    );

    // The new content is readable, so `content` really did store a blob rather than only a name.
    let rewritten = f
        .call("blob.get", json!({ "content_ref": head_entries[0].1 }))
        .await;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(rewritten["bytes"].as_str().unwrap())
        .expect("decode");
    assert_eq!(bytes, b"ONE");
}

#[tokio::test]
async fn tree_put_deletes_only_paths_the_tree_holds() {
    let f = fixture();
    let base = f.tree(&[("a", b"1", 644), ("b", b"2", 644)]).await;
    let out = f
        .call(
            "exec.tree_put",
            json!({ "tree": base, "edits": [{ "path": "a", "delete": true }] }),
        )
        .await;
    let left = entries_of(&f, out["tree"].as_str().unwrap()).await;
    assert_eq!(
        left.iter().map(|(p, _, _)| p.as_str()).collect::<Vec<_>>(),
        vec!["b"]
    );
    assert_eq!(out["entries"], json!(1));

    // A delete of a path the tree does not hold is refused rather than treated as done, because a
    // caller who believes it removed something is exactly the failure this verb must not produce.
    let err = f
        .call_err(
            "exec.tree_put",
            json!({ "tree": base, "edits": [{ "path": "absent", "delete": true }] }),
        )
        .await;
    assert!(err.contains("absent"), "the refusal names the path: {err}");
    // And a delete cannot carry a mode, which would otherwise read as a chmod that never happens.
    let err = f
        .call_err(
            "exec.tree_put",
            json!({ "tree": base, "edits": [{ "path": "a", "delete": true, "mode": 755 }] }),
        )
        .await;
    assert!(err.contains("mode"), "{err}");
}

#[tokio::test]
async fn tree_put_refuses_duplicate_paths_naming_both_edits() {
    let f = fixture();
    let base = f.tree(&[("a", b"1", 644)]).await;
    let err = f
        .call_err(
            "exec.tree_put",
            json!({ "tree": base, "edits": [
                { "path": "a", "content": "first" },
                { "path": "b", "content": "unrelated" },
                { "path": "a", "content": "second" },
            ]}),
        )
        .await;
    assert!(
        err.contains("edits[0]") && err.contains("edits[2]") && err.contains("\"a\""),
        "the refusal names both offending indices and the path: {err}"
    );
    // Last-one-wins is the behaviour being refused, so assert the tree did not quietly take one.
    assert_eq!(entries_of(&f, &base).await.len(), 1);
}

#[tokio::test]
async fn tree_put_refuses_an_empty_edit_list_rather_than_echoing_the_tree() {
    let f = fixture();
    let base = f.tree(&[("a", b"1", 644)]).await;
    let err = f
        .call_err("exec.tree_put", json!({ "tree": base, "edits": [] }))
        .await;
    assert!(err.contains("empty"), "{err}");
}

#[tokio::test]
async fn tree_put_refuses_edits_that_do_not_name_exactly_one_action() {
    let f = fixture();
    let base = f.tree(&[("a", b"1", 644)]).await;
    let r = f.put(b"x").await;
    for (edits, expect) in [
        (json!([{ "path": "a" }]), "0 of ref"),
        (
            json!([{ "path": "a", "ref": r, "content": "both" }]),
            "2 of ref",
        ),
        (
            json!([{ "path": "a", "ref": r, "delete": true }]),
            "2 of ref",
        ),
        (
            json!([{ "path": "a", "content": "x", "mode": 777 }]),
            "644 or 755",
        ),
        (json!([{ "path": "../escape", "content": "x" }]), "escape"),
        (
            json!([{ "path": "a", "content": "x", "mode": 0o644 }]),
            "644 or 755",
        ),
    ] {
        let err = f
            .call_err("exec.tree_put", json!({ "tree": base, "edits": edits }))
            .await;
        assert!(err.contains(expect), "expected {expect:?} in: {err}");
    }
    // A mode written as an octal literal is 420, not 644, and must be refused rather than stored:
    // `exec.tree` would later reject the manifest this verb had already minted.
    assert_eq!(entries_of(&f, &base).await, entries_of(&f, &base).await);
}

#[tokio::test]
async fn tree_put_refuses_a_file_that_would_become_a_directory_prefix() {
    let f = fixture();
    // `a` is a file. Adding `a/b` would make the manifest name `a` as both a file and a directory,
    // which `exec.tree` refuses on the way in; `tree_put` builds entries directly, so without
    // routing the candidate manifest through the same validator it could mint one `exec.tree_get`
    // would then refuse to load.
    let base = f.tree(&[("a", b"1", 644)]).await;
    let err = f
        .call_err(
            "exec.tree_put",
            json!({ "tree": base, "edits": [{ "path": "a/b", "content": "nested" }] }),
        )
        .await;
    assert!(!err.is_empty(), "a file cannot also be a directory prefix");
    // The inverse: replacing the file with the directory in one call is legitimate, because the
    // candidate manifest the validator sees no longer holds `a` as a file.
    let out = f
        .call(
            "exec.tree_put",
            json!({ "tree": base, "edits": [
                { "path": "a", "delete": true },
                { "path": "a/b", "content": "nested" },
            ]}),
        )
        .await;
    assert_eq!(
        entries_of(&f, out["tree"].as_str().unwrap())
            .await
            .iter()
            .map(|(p, _, _)| p.as_str())
            .collect::<Vec<_>>(),
        vec!["a/b"]
    );
}

#[tokio::test]
async fn tree_put_that_refuses_stores_nothing_including_blobs_for_the_good_entries() {
    let f = fixture();
    let base = f.tree(&[("a", b"1", 644)]).await;
    let before = blob_object_count(&f);
    assert!(
        before > 0,
        "the store is non-empty, so a count of zero would be an instrument fault"
    );

    // The first two entries are fine and carry content that is not yet in the store. The LAST one
    // fails normalization. Atomicity is a property of the result: one call yields exactly one new
    // tree or none, and a refusal stores nothing, including the blobs for the entries that were
    // fine. An implementation that put blobs as it validated them would pass every other arm here.
    let err = f
        .call_err(
            "exec.tree_put",
            json!({ "tree": base, "edits": [
                { "path": "good-one", "content": "content that appears in no other test" },
                { "path": "good-two", "content": "a second body unique to this arm" },
                { "path": "/absolute", "content": "the entry that fails normalization" },
            ]}),
        )
        .await;
    assert!(!err.is_empty());
    assert_eq!(
        blob_object_count(&f),
        before,
        "a refused put left a new object in the blob store"
    );

    // Positive control on the same instrument: the identical list minus the bad entry DOES store
    // its blobs, so the count above is measuring something that can move.
    let out = f
        .call(
            "exec.tree_put",
            json!({ "tree": base, "edits": [
                { "path": "good-one", "content": "content that appears in no other test" },
                { "path": "good-two", "content": "a second body unique to this arm" },
            ]}),
        )
        .await;
    assert!(out["tree"].is_string());
    assert!(
        blob_object_count(&f) > before,
        "the control must move the count the refusal arm asserts is still"
    );
}

#[tokio::test]
async fn tree_put_refuses_a_ref_that_names_no_stored_object_before_writing_anything() {
    let f = fixture();
    let base = f.tree(&[("a", b"1", 644)]).await;
    let before = blob_object_count(&f);
    let absent = "0".repeat(64);
    let err = f
        .call_err(
            "exec.tree_put",
            json!({ "tree": base, "edits": [
                { "path": "written-first", "content": "a body unique to the dangling-ref arm" },
                { "path": "dangling", "ref": absent },
            ]}),
        )
        .await;
    assert!(!err.is_empty());
    assert_eq!(
        blob_object_count(&f),
        before,
        "the good entry's blob was stored anyway"
    );
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
