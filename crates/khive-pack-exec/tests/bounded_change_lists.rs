//! ADR-181 Amendment 18 acceptance: bounded change lists (92 to 97, 99) and
//! directory deletes (98), driven through the verb registry against a
//! file-backed runtime with a real blob store. The arms that launch a process
//! use `/bin/sh` and only run on macOS, where `sandbox-exec` exists.

use khive_pack_blob::BlobPack;
use khive_pack_exec::ExecPack;
use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::engine_config::ExecSectionConfig;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::BlobStore;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const BYTE_BOUND: usize = 262_144;

struct Fixture {
    registry: VerbRegistry,
    blobs: PathBuf,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = RuntimeConfig {
        db_path: Some(dir.path().join("khive.db")),
        exec: ExecSectionConfig {
            root: Some(dir.path().join("exec-root").to_string_lossy().to_string()),
            read_roots: vec!["/bin".into(), "/usr/bin".into()],
            max_output_bytes: Some(4096),
            timeout_default_s: Some(60.0),
            timeout_max_s: Some(120.0),
            ..ExecSectionConfig::default()
        },
        ..RuntimeConfig::no_embeddings()
    };
    let rt = KhiveRuntime::new(cfg).expect("file runtime");
    let blobs = dir.path().join("blobs");
    let store: Arc<dyn BlobStore> = Arc::new(
        khive_db::stores::blob::FsBlobStore::new(blobs.clone(), 0).expect("fs blob store"),
    );
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
        blobs,
        _dir: dir,
    }
}

fn object_count(path: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            if entry.file_type().unwrap().is_dir() {
                object_count(&entry.path())
            } else {
                1
            }
        })
        .sum()
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

    fn objects(&self) -> usize {
        object_count(&self.blobs)
    }

    async fn put(&self, bytes: &[u8]) -> String {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let v = self.call("blob.put", json!({ "bytes": encoded })).await;
        v["content_ref"].as_str().unwrap().to_string()
    }

    /// A tree whose entries are `(path, bytes, mode)`.
    async fn tree(&self, files: &[(String, Vec<u8>, u32)]) -> String {
        let mut entries = Vec::new();
        for (path, bytes, mode) in files {
            let r = self.put(bytes).await;
            entries.push(json!({ "path": path, "ref": r, "mode": mode }));
        }
        let v = self.call("exec.tree", json!({ "entries": entries })).await;
        v["tree"].as_str().unwrap().to_string()
    }

    /// The full list behind `changed_ref`, read the way Amendment 18 item 6 says: `blob.get` with
    /// `range` from offset 0, adding each reply's `range.length`, until the offset is the size.
    async fn read_list(&self, changed_ref: &Value) -> Vec<Value> {
        use base64::Engine;
        const SLICE: u64 = 64 * 1024;
        let mut offset = 0u64;
        let mut bytes = Vec::new();
        loop {
            let v = self
                .call(
                    "blob.get",
                    json!({ "content_ref": changed_ref, "range": { "offset": offset, "length": SLICE } }),
                )
                .await;
            bytes.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(v["bytes"].as_str().unwrap())
                    .unwrap(),
            );
            offset += v["range"]["length"].as_u64().unwrap();
            if offset == v["size"].as_u64().unwrap() {
                break;
            }
        }
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.ends_with('\n'), "every line ends in a newline");
        text.split_terminator('\n')
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

fn files(paths: impl IntoIterator<Item = String>) -> Vec<(String, Vec<u8>, u32)> {
    paths.into_iter().map(|p| (p, b"x".to_vec(), 644)).collect()
}

fn list(reply: &Value) -> &Vec<Value> {
    reply["changed"].as_array().unwrap()
}

fn line_bytes(changes: &[Value]) -> usize {
    changes.iter().map(|c| c.to_string().len() + 1).sum()
}

fn short_paths(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("d/f{i:04}")).collect()
}

/// 400 characters, in components short enough for a real file system.
fn long_path(i: usize) -> String {
    let path = format!("d/{i:04}/{}/{}", "a".repeat(195), "b".repeat(197));
    assert_eq!(path.len(), 400);
    path
}

// 92
#[tokio::test]
async fn a_reply_carries_at_most_the_limit_and_says_what_it_cut() {
    let f = fixture();
    let base = f.tree(&files(short_paths(1500))).await;
    let edit = json!([{ "path": "d", "delete": true, "recursive": true }]);
    let reply = f
        .call("exec.tree_put", json!({ "tree": base, "edits": edit }))
        .await;
    assert_eq!(list(&reply).len(), 1000);
    assert_eq!(reply["changed_count"], 1500);
    assert_eq!(reply["changed_truncated"], true);
    assert!(reply["changed_ref"].is_string(), "{}", reply["changed_ref"]);
    assert_eq!(reply["entries"], 0);

    let whole = f
        .call(
            "exec.tree_put",
            json!({ "tree": base, "edits": edit, "changed_limit": 1500 }),
        )
        .await;
    assert_eq!(list(&whole).len(), 1500);
    assert_eq!(whole["changed_count"], 1500);
    assert_eq!(whole["changed_truncated"], false);
    assert_eq!(whole["changed_ref"], reply["changed_ref"]);
    assert_eq!(list(&whole)[..1000], list(&reply)[..]);
}

// 93
#[tokio::test]
async fn a_reply_stops_before_the_change_that_would_cross_the_byte_bound() {
    let f = fixture();
    let base = f.tree(&files((0..1000).map(long_path))).await;
    let reply = f
        .call(
            "exec.tree_put",
            json!({ "tree": base, "edits": [{ "path": "d", "delete": true, "recursive": true }] }),
        )
        .await;
    let kept = list(&reply);
    assert!(kept.len() < 1000, "{}", kept.len());
    assert_eq!(reply["changed_count"], 1000);
    assert_eq!(reply["changed_truncated"], true);
    let full = f.read_list(&reply["changed_ref"]).await;
    assert_eq!(full.len(), 1000);
    assert_eq!(
        full[..kept.len()],
        kept[..],
        "the reply is a prefix of the blob"
    );
    let used = line_bytes(kept);
    assert!(used <= BYTE_BOUND, "{used}");
    assert!(
        used + line_bytes(&full[kept.len()..=kept.len()]) > BYTE_BOUND,
        "the next change would cross the bound"
    );
}

// 94
#[tokio::test]
async fn changed_limit_above_the_ceiling_is_refused_before_any_write() {
    let f = fixture();
    let base = f.tree(&files(short_paths(3))).await;
    let edits = json!([{ "path": "d", "delete": true, "recursive": true }]);
    let before = f.objects();
    let refused = f
        .call_err(
            "exec.tree_put",
            json!({ "tree": base, "edits": edits, "changed_limit": 10_001 }),
        )
        .await;
    assert!(refused.contains("10,000"), "{refused}");
    assert_eq!(f.objects(), before, "a refused put writes nothing");
    for bad in [json!(-1), json!("5"), json!(1.5)] {
        let refused = f
            .call_err(
                "exec.tree_put",
                json!({ "tree": base, "edits": edits, "changed_limit": bad }),
            )
            .await;
        assert!(refused.contains("changed_limit"), "{refused}");
        assert_eq!(f.objects(), before);
    }
    let head = f
        .call(
            "exec.tree_put",
            json!({ "tree": base, "edits": edits, "changed_limit": 10_000 }),
        )
        .await;
    assert_eq!(list(&head).len(), 3);
    for (verb, args) in [
        (
            "exec.tree_diff",
            json!({ "base": base, "head": head["tree"] }),
        ),
        ("exec.receipt", json!({ "id": "no-such-receipt" })),
        ("exec.runs", json!({ "actor": "local" })),
        (
            "exec.run",
            json!({ "tree": base, "tool": "sh", "actor": "local" }),
        ),
    ] {
        let mut args = args;
        args["changed_limit"] = json!(10_001);
        let refused = f.call_err(verb, args).await;
        assert!(refused.contains("10,000"), "{verb}: {refused}");
    }
}

// 95, the tree arm
#[tokio::test]
async fn the_full_list_of_a_recursive_delete_is_read_back_through_blob_get() {
    let f = fixture();
    let base = f.tree(&files(short_paths(1200))).await;
    let reply = f
        .call(
            "exec.tree_put",
            json!({ "tree": base, "edits": [{ "path": "d", "delete": true, "recursive": true }] }),
        )
        .await;
    assert_eq!(list(&reply).len(), 1000);
    assert_eq!(reply["changed_truncated"], true);
    let full = f.read_list(&reply["changed_ref"]).await;
    let diff = f
        .call(
            "exec.tree_diff",
            json!({ "base": base, "head": reply["tree"], "changed_limit": 10_000 }),
        )
        .await;
    assert_eq!(diff["changed_truncated"], false);
    assert_eq!(&full, list(&diff));
    assert_eq!(full.len(), 1200);
    assert!(full.iter().all(|c| c["op"] == "deleted"));
}

// 97
#[tokio::test]
async fn tree_diff_writes_the_list_only_when_it_cuts_it() {
    let f = fixture();
    let small = f.tree(&files(short_paths(3))).await;
    let empty = f.tree(&[]).await;
    let before = f.objects();
    let under = f
        .call("exec.tree_diff", json!({ "base": small, "head": empty }))
        .await;
    assert_eq!(list(&under).len(), 3);
    assert_eq!(under["changed_truncated"], false);
    assert_eq!(under["changed_ref"], Value::Null);
    assert_eq!(f.objects(), before, "a diff under the bound writes nothing");

    let base = f.tree(&files(short_paths(1500))).await;
    let before = f.objects();
    let over = f
        .call("exec.tree_diff", json!({ "base": base, "head": empty }))
        .await;
    assert_eq!(over["changed_truncated"], true);
    assert_eq!(f.objects(), before + 1, "a cut diff writes one object");
    // The same list from exec.tree_put has the same reference: one list, one blob.
    let put = f
        .call(
            "exec.tree_put",
            json!({ "tree": base, "edits": [{ "path": "d", "delete": true, "recursive": true }] }),
        )
        .await;
    assert_eq!(put["tree"], json!(empty));
    assert_eq!(put["changed_ref"], over["changed_ref"]);
    assert_eq!(f.objects(), before + 1);
}

// 98
#[tokio::test]
async fn a_recursive_delete_removes_by_path_component_and_follows_no_link() {
    let f = fixture();
    let mut entries = files(
        [
            "dir/a",
            "dir/sub/b",
            "dir-file",
            "dir.txt",
            "dir2/x",
            "keep",
        ]
        .into_iter()
        .map(String::from),
    );
    entries.push(("dir/l".into(), b"../keep".to_vec(), 120000));
    let base = f.tree(&entries).await;
    let reply = f
        .call(
            "exec.tree_put",
            json!({ "tree": base, "edits": [{ "path": "dir", "delete": true, "recursive": true }] }),
        )
        .await;
    let removed: Vec<&str> = list(&reply)
        .iter()
        .map(|c| {
            assert_eq!(c["op"], "deleted");
            c["path"].as_str().unwrap()
        })
        .collect();
    assert_eq!(removed, ["dir/a", "dir/l", "dir/sub/b"]);
    let kept: Vec<String> = f
        .call("exec.tree_get", json!({ "tree": reply["tree"] }))
        .await["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(kept, ["dir-file", "dir.txt", "dir2/x", "keep"]);
    let keep_ref = f.put(b"x").await;
    let after = f
        .call("exec.tree_get", json!({ "tree": reply["tree"] }))
        .await;
    assert!(after["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["path"] == "keep" && e["ref"] == json!(keep_ref)));

    let before = f.objects();
    let refusals: Vec<(Value, &str)> = vec![
        (
            json!([{ "path": "keep", "delete": true, "recursive": true }]),
            "file or link entry",
        ),
        (
            json!([{ "path": "dir/l", "delete": true, "recursive": true }]),
            "file or link entry",
        ),
        (
            json!([{ "path": "", "delete": true, "recursive": true }]),
            "empty",
        ),
        (
            json!([{ "path": ".", "delete": true, "recursive": true }]),
            "not normalized",
        ),
        (
            json!([{ "path": "/", "delete": true, "recursive": true }]),
            "absolute",
        ),
        (
            json!([{ "path": "dir/", "delete": true, "recursive": true }]),
            "not normalized",
        ),
        (
            json!([{ "path": "./dir", "delete": true, "recursive": true }]),
            "not normalized",
        ),
        (
            json!([{ "path": "dir/../dir", "delete": true, "recursive": true }]),
            "not normalized",
        ),
        (
            json!([{ "path": "nothing", "delete": true, "recursive": true }]),
            "no entry under it",
        ),
        (
            json!([{ "path": "dir", "content": "x", "recursive": true }]),
            "not a delete",
        ),
        (
            json!([{ "path": "dir", "delete": true, "recursive": "yes" }]),
            "recursive must be a boolean",
        ),
        (json!([{ "path": "dir", "delete": true }]), "recursive"),
        (
            json!([{ "path": "dir", "delete": true, "recursive": true }, { "path": "dir/a", "delete": true }]),
            "edits[0] and edits[1]",
        ),
        (
            json!([{ "path": "dir/new", "content": "n" }, { "path": "dir", "delete": true, "recursive": true }]),
            "edits[0] and edits[1]",
        ),
        (
            json!([{ "path": "keep", "content": "k" }, { "path": "dir/sub", "delete": true, "recursive": true }, { "path": "dir/sub/b", "delete": true }]),
            "edits[1] and edits[2]",
        ),
    ];
    for (edits, expected) in refusals {
        let refused = f
            .call_err("exec.tree_put", json!({ "tree": base, "edits": edits }))
            .await;
        assert!(refused.contains(expected), "{edits}: {refused}");
        assert_eq!(f.objects(), before, "{edits}: a refusal writes nothing");
    }
    // A plain file delete and `recursive: false` are unchanged.
    let plain = f
        .call(
            "exec.tree_put",
            json!({ "tree": base, "edits": [{ "path": "keep", "delete": true, "recursive": false }] }),
        )
        .await;
    assert_eq!(list(&plain).len(), 1);
}

// 99, the tree arms
#[tokio::test]
async fn a_small_reply_is_todays_reply_plus_three_fields() {
    let f = fixture();
    let base = f.tree(&files(short_paths(3))).await;
    let put = f
        .call(
            "exec.tree_put",
            json!({ "tree": base, "edits": [{ "path": "d", "delete": true, "recursive": true }] }),
        )
        .await;
    let mut keys: Vec<&str> = put
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "base",
            "changed",
            "changed_count",
            "changed_ref",
            "changed_truncated",
            "entries",
            "tree"
        ]
    );
    assert_eq!(put["changed_count"], 3);
    assert_eq!(put["changed_truncated"], false);
    assert!(put["changed_ref"].is_string());
    assert_eq!(&f.read_list(&put["changed_ref"]).await, list(&put));
    let diff = f
        .call(
            "exec.tree_diff",
            json!({ "base": base, "head": put["tree"] }),
        )
        .await;
    let mut keys: Vec<&str> = diff
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "base",
            "changed",
            "changed_count",
            "changed_ref",
            "changed_truncated",
            "head"
        ]
    );
    assert_eq!(diff["changed"], put["changed"]);
    assert_eq!(diff["changed_ref"], Value::Null);
    // An empty list has no blob.
    let same = f
        .call("exec.tree_diff", json!({ "base": base, "head": base }))
        .await;
    assert_eq!(same["changed"], json!([]));
    assert_eq!(same["changed_count"], 0);
    assert_eq!(same["changed_ref"], Value::Null);
}

#[cfg(target_os = "macos")]
mod runs {
    use super::*;

    async fn register_sh(f: &Fixture) {
        f.call(
            "tool.register",
            json!({
                "name": "sh", "kind": "tool", "description": "shell",
                "source": "exec:/bin/sh", "side_effect": "write", "trust": "first_party",
            }),
        )
        .await;
        f.call(
            "tool.policy",
            json!({ "actor": "*", "tool": "sh", "decision": "allow" }),
        )
        .await;
    }

    async fn delete_d(f: &Fixture, tree: &str, session: &str) -> Value {
        f.call(
            "exec.run",
            json!({
                "tree": tree, "tool": "sh", "args": ["-c", "rm -rf d"], "actor": "local",
                "declared_write_paths": ["d"], "session_id": session,
            }),
        )
        .await
    }

    // 95, the run arm, and 96
    #[tokio::test]
    async fn a_run_reply_carries_one_bounded_list_twice_and_the_row_keeps_it_whole() {
        let f = fixture();
        register_sh(&f).await;
        let tree = f.tree(&files(short_paths(1200))).await;
        let out = delete_d(&f, &tree, "one").await;
        let receipt = &out["receipt"];
        assert_eq!(receipt["exit_code"], 0, "{receipt}");
        for key in [
            "changed",
            "changed_count",
            "changed_truncated",
            "changed_ref",
        ] {
            assert_eq!(out[key], receipt[key], "{key}");
        }
        assert_eq!(list(&out).len(), 1000);
        assert_eq!(out["changed_count"], 1200);
        assert_eq!(out["changed_truncated"], true);
        let full = f.read_list(&out["changed_ref"]).await;
        assert_eq!(full.len(), 1200);

        let before = f.objects();
        let stored = f
            .call(
                "exec.receipt",
                json!({ "id": receipt["id"], "changed_limit": 10_000 }),
            )
            .await;
        assert_eq!(stored["changed_truncated"], false);
        assert_eq!(&full, list(&stored), "the route gives the persisted list");
        let bounded = f.call("exec.receipt", json!({ "id": receipt["id"] })).await;
        assert_eq!(bounded["changed"], out["changed"]);
        assert_eq!(bounded["changed_ref"], out["changed_ref"]);
        assert_eq!(bounded["changed_count"], 1200);
        let runs = f
            .call(
                "exec.runs",
                json!({ "actor": "local", "session_id": "one" }),
            )
            .await;
        assert_eq!(runs["runs"][0]["changed"], out["changed"]);
        assert_eq!(runs["runs"][0]["changed_ref"], out["changed_ref"]);
        assert_eq!(f.objects(), before, "the readers write nothing");
    }

    // 96, the exec.runs arm
    #[tokio::test]
    async fn runs_share_one_byte_bound_across_the_whole_reply() {
        let f = fixture();
        register_sh(&f).await;
        let tree = f.tree(&files((0..1000).map(long_path))).await;
        for _ in 0..3 {
            let out = delete_d(&f, &tree, "many").await;
            assert_eq!(out["receipt"]["exit_code"], 0, "{}", out["receipt"]);
            assert_eq!(out["changed_truncated"], true, "each list fills the bound");
        }
        let runs = f
            .call(
                "exec.runs",
                json!({ "actor": "local", "session_id": "many" }),
            )
            .await;
        let rows = runs["runs"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        let spent: usize = rows
            .iter()
            .map(|row| line_bytes(row["changed"].as_array().unwrap()))
            .sum();
        assert!(spent <= BYTE_BOUND, "{spent}");
        assert!(!rows[0]["changed"].as_array().unwrap().is_empty());
        for row in rows {
            assert_eq!(row["changed_count"], 1000);
            assert_eq!(row["changed_truncated"], true);
            assert!(row["changed_ref"].is_string());
        }
        for row in &rows[1..] {
            assert_eq!(row["changed"], json!([]));
        }
    }

    // 99, the run arm
    #[tokio::test]
    async fn a_small_run_reply_is_todays_reply_plus_three_fields() {
        let f = fixture();
        register_sh(&f).await;
        let tree = f.tree(&files(short_paths(3))).await;
        let out = delete_d(&f, &tree, "small").await;
        let mut keys: Vec<&str> = out
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "changed",
                "changed_count",
                "changed_ref",
                "changed_truncated",
                "receipt"
            ]
        );
        assert_eq!(list(&out).len(), 3);
        assert_eq!(out["changed_truncated"], false);
        assert_eq!(&f.read_list(&out["changed_ref"]).await, list(&out));
        assert_eq!(out["receipt"]["changed"], out["changed"]);

        let stored = f
            .call("exec.receipt", json!({ "id": out["receipt"]["id"] }))
            .await;
        let runs = f
            .call(
                "exec.runs",
                json!({ "actor": "local", "session_id": "small" }),
            )
            .await;
        for row in [&stored, &runs["runs"][0]] {
            assert_eq!(row["changed"], out["changed"], "{row}");
            assert_eq!(row["changed_count"], 3, "{row}");
            assert_eq!(row["changed_truncated"], false, "{row}");
            assert_eq!(row["changed_ref"], out["changed_ref"], "{row}");
        }
    }
}
