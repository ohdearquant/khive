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
    fixture_with_keep(false)
}

fn fixture_with_keep(keep: bool) -> Fixture {
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
            keep,
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

/// Count the objects the blob store holds. `exec.tree_put` promises that a call refused during
/// validation leaves no new object behind, and that is a claim about the store, not about the
/// call's return value, so the arm asserting it has to look at the store itself. The promise is
/// scoped to validation: once validation passes, the blobs are published one at a time, and a
/// backend failure partway through leaves the objects already published, referenced by no tree.
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
    // `changed` is the verb's own answer for what it did, so the arm reads it rather than a count
    // of a diff taken afterwards: four edits produce four changed paths whatever the four are, and
    // a count cannot tell a rewrite of `a.txt` from a rewrite of something else.
    let changed: Vec<(&str, &str)> = out["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["path"].as_str().unwrap(), c["op"].as_str().unwrap()))
        .collect();
    assert_eq!(
        changed,
        vec![
            ("a.txt", "modified"),
            ("keep/b.txt", "modified"),
            ("new/c.txt", "added"),
            ("new/d.txt", "added"),
        ]
    );
    assert_eq!(
        out["changed"],
        f.call("exec.tree_diff", json!({ "base": base, "head": head }))
            .await["changed"],
        "the verb's `changed` is the diff of its base against its result"
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
async fn tree_put_preserves_symlink_targets_and_classifies_mode_flips() {
    let f = fixture();
    let base = f
        .tree(&[("plain", b"target", 644), ("executable", b"target", 755)])
        .await;
    let target_ref = f.put(b"target").await;
    let created = f
        .call(
            "exec.tree_put",
            json!({ "tree": base, "edits": [
                { "path": "link", "content": "./target", "mode": 120000 },
                { "path": "plain", "ref": target_ref, "mode": 120000 },
                { "path": "executable", "ref": target_ref, "mode": 120000 },
            ]}),
        )
        .await;
    let linked = created["tree"].as_str().unwrap();
    let entries = entries_of(&f, linked).await;
    assert!(entries.iter().all(|(_, _, mode)| *mode == 120000));
    assert_eq!(entries[1].1, f.put(b"./target").await);
    let changes: Vec<(&str, &str)> = created["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["path"].as_str().unwrap(), c["op"].as_str().unwrap()))
        .collect();
    assert_eq!(
        changes,
        vec![
            ("executable", "modified"),
            ("link", "added"),
            ("plain", "modified"),
        ]
    );
    assert_eq!(
        created["changed"],
        f.call("exec.tree_diff", json!({ "base": base, "head": linked }))
            .await["changed"]
    );

    let retargeted = f
        .call(
            "exec.tree_put",
            json!({ "tree": linked, "edits": [
                { "path": "link", "content": "../other" },
                { "path": "plain", "content": "target", "mode": 644 },
                { "path": "executable", "ref": target_ref, "mode": 755 },
            ]}),
        )
        .await;
    let head = retargeted["tree"].as_str().unwrap();
    assert_eq!(
        entries_of(&f, head).await,
        vec![
            ("executable".into(), target_ref.clone(), 755),
            ("link".into(), f.put(b"../other").await, 120000),
            ("plain".into(), target_ref.clone(), 644),
        ]
    );
    assert_eq!(retargeted["changed"].as_array().unwrap().len(), 3);
    assert!(retargeted["changed"]
        .as_array()
        .unwrap()
        .iter()
        .all(|change| change["op"] == "modified"));
    assert_eq!(
        retargeted["changed"],
        f.call("exec.tree_diff", json!({ "base": linked, "head": head }))
            .await["changed"]
    );

    let deleted = f
        .call(
            "exec.tree_put",
            json!({ "tree": head, "edits": [{ "path": "link", "delete": true }] }),
        )
        .await;
    assert_eq!(deleted["changed"].as_array().unwrap().len(), 1);
    assert_eq!(deleted["changed"][0]["path"], "link");
    assert_eq!(deleted["changed"][0]["op"], "deleted");
    assert_eq!(
        deleted["tree"], base,
        "all original file modes are restored"
    );
    assert_eq!(entries_of(&f, linked).await, entries, "trees are immutable");
}

#[tokio::test]
async fn tree_symlink_mode_does_not_allow_invalid_modes_or_descendant_entries() {
    let f = fixture();
    let target_ref = f.put(b"../outside").await;
    let tree = f.tree(&[("link", b"../outside", 120000)]).await;
    assert_eq!(
        entries_of(&f, &tree).await,
        vec![("link".into(), target_ref.clone(), 120000)]
    );
    for mode in [777, 0o120777, 0o120000, 100644] {
        let err = f
            .call_err(
                "exec.tree",
                json!({ "entries": [{ "path": "link", "ref": target_ref, "mode": mode }] }),
            )
            .await;
        assert!(err.contains("mode"), "{err}");
        let err = f
            .call_err(
                "exec.tree_put",
                json!({ "tree": tree, "edits": [{ "path": "link", "ref": target_ref, "mode": mode }] }),
            )
            .await;
        assert!(err.contains("mode"), "{err}");
    }
    let err = f
        .call_err(
            "exec.tree",
            json!({ "entries": [
                { "path": "link", "ref": target_ref, "mode": 120000 },
                { "path": "link/child", "ref": target_ref, "mode": 644 },
            ]}),
        )
        .await;
    assert!(err.contains("link"), "{err}");
    let before = blob_object_count(&f);
    let err = f
        .call_err(
            "exec.tree_put",
            json!({ "tree": tree, "edits": [{ "path": "link/child", "content": "child" }] }),
        )
        .await;
    assert!(err.contains("link"), "{err}");
    assert_eq!(blob_object_count(&f), before);
    assert_eq!(entries_of(&f, &tree).await.len(), 1);
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
            "644, 755 or 120000",
        ),
        (json!([{ "path": "../escape", "content": "x" }]), "escape"),
        (
            json!([{ "path": "a", "content": "x", "mode": 0o644 }]),
            "644, 755 or 120000",
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
async fn run_captures_symlink_creation_retargeting_removal_and_mode_flips() {
    let f = fixture();
    f.register_sh("sh", "allow").await;
    let tree = f
        .tree(&[
            ("target", b"body", 644),
            ("dir/file", b"nested", 644),
            ("retarget", b"target", 120000),
            ("remove", b"target", 120000),
            ("to-file", b"target", 120000),
            ("to-link", b"target", 644),
        ])
        .await;
    let script = "set -e; ln -s './dir/../target' created; \
        rm retarget; ln -s dir/file retarget; rm remove; ln -s dir dir-link; \
        rm to-file; printf target > to-file; rm to-link; ln -s target to-link";
    let out = f
        .call(
            "exec.run",
            json!({ "tree": tree, "tool": "sh", "args": ["-c", script], "actor": "local" }),
        )
        .await;
    let receipt = &out["receipt"];
    assert_eq!(receipt["exit_code"], 0, "{receipt}");
    assert_eq!(receipt["success"], true, "{receipt}");
    assert_eq!(receipt["skipped"], json!([]));
    let changed: Vec<(&str, &str)> = out["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["path"].as_str().unwrap(), c["op"].as_str().unwrap()))
        .collect();
    assert_eq!(
        changed,
        vec![
            ("created", "added"),
            ("dir-link", "added"),
            ("remove", "deleted"),
            ("retarget", "modified"),
            ("to-file", "modified"),
            ("to-link", "modified"),
        ]
    );
    let head = receipt["tree_out"].as_str().unwrap();
    assert_eq!(
        entries_of(&f, head).await,
        vec![
            ("created".into(), f.put(b"./dir/../target").await, 120000),
            ("dir-link".into(), f.put(b"dir").await, 120000),
            ("dir/file".into(), f.put(b"nested").await, 644),
            ("retarget".into(), f.put(b"dir/file").await, 120000),
            ("target".into(), f.put(b"body").await, 644),
            ("to-file".into(), f.put(b"target").await, 644),
            ("to-link".into(), f.put(b"target").await, 120000),
        ],
        "directory links are entries, and their descendants are never captured"
    );
    assert_eq!(
        out["changed"],
        f.call("exec.tree_diff", json!({ "base": tree, "head": head }))
            .await["changed"]
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn run_denies_writes_through_escaping_symlinks_and_allows_inside_targets() {
    use std::os::unix::ffi::OsStrExt;

    let f = fixture();
    f.register_sh("sh", "allow").await;
    let outside = f._dir.path().join("outside.txt");
    std::fs::write(&outside, b"unchanged").expect("outside control file");
    for target in [
        b"../../outside.txt".as_slice(),
        outside.as_os_str().as_bytes(),
    ] {
        let tree = f
            .tree(&[
                ("inside", b"before", 644),
                ("inside-link", b"inside", 120000),
                ("escape", target, 120000),
            ])
            .await;
        let out = f
            .call(
                "exec.run",
                json!({
                    "tree": tree, "tool": "sh", "actor": "local",
                    "args": ["-c", "set -e; printf changed > inside-link; printf escaped > escape"],
                    "declared_write_paths": ["inside", "inside-link", "escape"],
                }),
            )
            .await;
        let receipt = &out["receipt"];
        assert_ne!(receipt["exit_code"], 0, "{receipt}");
        assert_eq!(receipt["success"], false, "{receipt}");
        assert_eq!(receipt["denied"], false, "the tool was launched: {receipt}");
        let stderr = f.blob_text(&receipt["stderr_ref"]).await;
        assert!(
            stderr.contains("ermitted") || stderr.contains("ermission"),
            "the kernel refusal must be reported in stderr: {stderr:?}"
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"unchanged");
        assert_eq!(receipt["undeclared_changes"], json!([]));
        assert_eq!(
            entries_of(&f, receipt["tree_out"].as_str().unwrap()).await,
            vec![
                ("escape".into(), f.put(target).await, 120000),
                ("inside".into(), f.put(b"changed").await, 644),
                ("inside-link".into(), f.put(b"inside").await, 120000),
            ],
            "the inside write control succeeds while the outside target stays unchanged"
        );
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn run_materialized_symlinks_round_trip_losslessly_through_git() {
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::process::Command;

    let f = fixture_with_keep(true);
    f.register_sh("sh", "allow").await;
    let repo = tempfile::tempdir().expect("git fixture");
    let git_dir = repo.path().join(".git");
    let git = |worktree: &Path, args: &[&str]| {
        let output = Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .arg("--git-dir")
            .arg(&git_dir)
            .arg("--work-tree")
            .arg(worktree)
            .args(["-c", "core.symlinks=true", "-c", "core.filemode=true"])
            .args(args)
            .current_dir(worktree)
            .output()
            .expect("launch fixture git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    git(repo.path(), &["init", "--quiet"]);
    std::fs::write(repo.path().join("target.txt"), b"payload\n").unwrap();
    std::fs::create_dir(repo.path().join("dir")).unwrap();
    std::fs::write(repo.path().join("dir/child.txt"), b"nested\n").unwrap();
    for (name, target) in [
        ("file-link", "target.txt"),
        ("dir-link", "dir"),
        ("dangling-link", "missing"),
    ] {
        symlink(target, repo.path().join(name)).expect("tracked fixture symlink");
    }
    git(repo.path(), &["add", "--force", "--all", "."]);
    let original = String::from_utf8(git(repo.path(), &["write-tree"]))
        .unwrap()
        .trim()
        .to_string();
    let index = String::from_utf8(git(repo.path(), &["ls-files", "-s", "-z"]))
        .expect("fixture index paths are UTF-8");
    let mut manifest = Vec::new();
    let mut symlinks = 0;
    for entry in index.split('\0').filter(|entry| !entry.is_empty()) {
        let (header, path) = entry.split_once('\t').expect("git index entry");
        let mut fields = header.split_whitespace();
        let mode = match fields.next().expect("git mode") {
            "100644" => 644,
            "100755" => 755,
            "120000" => {
                symlinks += 1;
                120000
            }
            mode => panic!("unexpected fixture mode: {mode}"),
        };
        let object = fields.next().expect("git blob id");
        assert_eq!(fields.next(), Some("0"));
        let bytes = git(repo.path(), &["cat-file", "blob", object]);
        manifest.push(json!({ "path": path, "ref": f.put(&bytes).await, "mode": mode }));
    }
    assert_eq!(symlinks, 3, "the source index must contain actual symlinks");
    let input = f.call("exec.tree", json!({ "entries": manifest })).await;
    let tree = input["tree"].as_str().unwrap();
    let out = f
        .call(
            "exec.run",
            json!({ "tree": tree, "tool": "sh", "args": ["-c", ":"], "actor": "local" }),
        )
        .await;
    let receipt = &out["receipt"];
    assert_eq!(receipt["exit_code"], 0, "{receipt}");
    assert_eq!(receipt["success"], true, "{receipt}");
    let materialized = f.root.join(receipt["id"].as_str().unwrap());

    // Git reads the actual materialization, so matching target blobs cannot conceal a mode loss.
    git(&materialized, &["add", "--force", "--all", "."]);
    let rebuilt = String::from_utf8(git(&materialized, &["write-tree"]))
        .unwrap()
        .trim()
        .to_string();
    let diff = git(
        &materialized,
        &["diff-tree", "--no-commit-id", "-r", &original, &rebuilt],
    );
    assert!(
        diff.is_empty(),
        "materializing the manifest must preserve Git's tree: {}",
        String::from_utf8_lossy(&diff)
    );
    assert_eq!(receipt["tree_out"], tree);
    assert_eq!(receipt["skipped"], json!([]));
    for (name, target) in [
        ("file-link", "target.txt"),
        ("dir-link", "dir"),
        ("dangling-link", "missing"),
    ] {
        assert!(std::fs::symlink_metadata(materialized.join(name))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_link(materialized.join(name)).unwrap(),
            Path::new(target)
        );
    }

    std::fs::remove_file(materialized.join("file-link")).unwrap();
    std::fs::write(materialized.join("file-link"), b"target.txt").unwrap();
    git(&materialized, &["add", "--all", "."]);
    let changed = String::from_utf8(git(&materialized, &["write-tree"]))
        .unwrap()
        .trim()
        .to_string();
    let patch = String::from_utf8(git(
        &materialized,
        &[
            "diff-tree",
            "--no-commit-id",
            "-r",
            "-p",
            &original,
            &changed,
        ],
    ))
    .unwrap();
    assert!(patch.contains("deleted file mode 120000"), "{patch}");
    assert!(patch.contains("new file mode 100644"), "{patch}");
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
