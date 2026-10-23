use super::*;
use khive_storage::BlobStore;

#[tokio::test]
async fn manifest_and_cwd_link_reads_wait_for_shared_hydration_admission() {
    let dir = tempfile::tempdir().unwrap();
    let rt = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: Some(dir.path().join("runtime.db")),
        blob_hydration_bytes: khive_storage::MAX_BLOB_WHOLE_BYTES,
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let blob_store: std::sync::Arc<dyn BlobStore> = std::sync::Arc::new(
        khive_db::stores::blob::FsBlobStore::new(dir.path().join("blobs"), 0).unwrap(),
    );
    rt.install_blob_store(blob_store.clone()).unwrap();
    let held_ref = blob_store.put(b"held".to_vec()).await.unwrap();
    let link_ref = blob_store.put(b"dir".to_vec()).await.unwrap();
    let file_ref = blob_store.put(b"file".to_vec()).await.unwrap();
    let entries = vec![
        TreeEntry {
            path: "dir/file".into(),
            content_ref: file_ref.as_str().into(),
            mode: 644,
        },
        TreeEntry {
            path: "link".into(),
            content_ref: link_ref.as_str().into(),
            mode: 120000,
        },
    ];
    let tree_ref = store(&rt, &entries).await.unwrap();
    let held = rt
        .blob_hydrator()
        .unwrap()
        .hydrate_verified(&held_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
        .await
        .unwrap();

    let load_rt = rt.clone();
    let mut load_task = tokio::spawn(async move { load(&load_rt, &tree_ref).await });
    let cwd_rt = rt.clone();
    let cwd_entries = entries.clone();
    let mut cwd_task =
        tokio::spawn(async move { resolve_cwd(&cwd_rt, &cwd_entries, "link").await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), &mut load_task)
            .await
            .is_err(),
        "manifest read must wait behind the held shared budget"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), &mut cwd_task)
            .await
            .is_err(),
        "cwd symlink read must wait behind the held shared budget"
    );

    drop(held);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), load_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        entries
    );
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), cwd_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        "dir"
    );
}

#[test]
fn rejects_unsafe_paths() {
    for bad in ["../x", "/abs", "a/../b", "", "a//b", "./a", "a\\b"] {
        assert!(validate_relative_path(bad, "entry").is_err(), "{bad:?}");
    }
    assert_eq!(
        validate_relative_path("a/b.txt", "entry").unwrap(),
        "a/b.txt"
    );
    assert_eq!(validate_cwd("").unwrap(), ".");
    assert!(validate_cwd("package/../package").is_err());
}

#[test]
fn cwd_component_cost_bounds_raw_paths_before_skipping_noops() {
    let bytes = "a".repeat(MAX_CWD_PATH_BYTES);
    assert_eq!(cwd_component_cost(&bytes).unwrap(), (1, bytes.len()));
    assert!(cwd_component_cost(&(bytes + "a")).is_err());

    let components = "/".repeat(MAX_CWD_PATH_COMPONENTS - 1);
    assert_eq!(cwd_component_cost(&components).unwrap(), (0, 0));
    assert!(cwd_component_cost(&(components + "/")).is_err());
}

#[test]
fn cwd_queue_bounds_components_before_mutation() {
    let mut pending = VecDeque::new();
    let mut bytes = 0;
    let path = vec!["a"; MAX_CWD_PATH_COMPONENTS].join("/");
    for _ in 0..MAX_CWD_PENDING_COMPONENTS / MAX_CWD_PATH_COMPONENTS {
        prepend_cwd_components(&mut pending, &mut bytes, &path).unwrap();
    }
    assert_eq!(pending.len(), MAX_CWD_PENDING_COMPONENTS);
    assert_eq!(bytes, MAX_CWD_PENDING_COMPONENTS);
    let before = pending.clone();
    let error = prepend_cwd_components(&mut pending, &mut bytes, "a")
        .unwrap_err()
        .to_string();
    assert!(error.contains("pending component budget"), "{error}");
    assert_eq!(pending, before);
    assert_eq!(bytes, MAX_CWD_PENDING_COMPONENTS);
}

#[test]
fn cwd_queue_bounds_bytes_before_mutation() {
    let mut pending = VecDeque::new();
    let mut bytes = 0;
    let path = "a".repeat(MAX_CWD_PATH_BYTES);
    for _ in 0..MAX_CWD_PENDING_BYTES / MAX_CWD_PATH_BYTES {
        prepend_cwd_components(&mut pending, &mut bytes, &path).unwrap();
    }
    assert_eq!(bytes, MAX_CWD_PENDING_BYTES);
    let before = pending.clone();
    let error = prepend_cwd_components(&mut pending, &mut bytes, "a")
        .unwrap_err()
        .to_string();
    assert!(error.contains("pending byte budget"), "{error}");
    assert_eq!(pending, before);
    assert_eq!(bytes, MAX_CWD_PENDING_BYTES);
}

#[test]
fn cwd_queue_skips_noops_without_reordering_parent_components() {
    let mut pending = VecDeque::new();
    let mut bytes = 0;
    prepend_cwd_components(&mut pending, &mut bytes, "tail").unwrap();
    prepend_cwd_components(&mut pending, &mut bytes, "first//./../second/.").unwrap();
    assert_eq!(
        pending,
        VecDeque::from(["first", "..", "second", "tail"].map(str::to_string))
    );
    assert_eq!(bytes, 17);
}

#[test]
fn diff_classifies() {
    let r1 = digest_hex(b"1");
    let r2 = digest_hex(b"2");
    let base = vec![
        TreeEntry {
            path: "keep".into(),
            content_ref: r1.clone(),
            mode: 644,
        },
        TreeEntry {
            path: "mod".into(),
            content_ref: r1.clone(),
            mode: 644,
        },
        TreeEntry {
            path: "del".into(),
            content_ref: r1.clone(),
            mode: 644,
        },
    ];
    let head = vec![
        TreeEntry {
            path: "keep".into(),
            content_ref: r1.clone(),
            mode: 644,
        },
        TreeEntry {
            path: "mod".into(),
            content_ref: r2.clone(),
            mode: 644,
        },
        TreeEntry {
            path: "add".into(),
            content_ref: r2.clone(),
            mode: 755,
        },
    ];
    let ops: Vec<(String, &str)> = diff(&base, &head)
        .into_iter()
        .map(|c| (c.path, c.op))
        .collect();
    assert_eq!(
        ops,
        vec![
            ("add".into(), "added"),
            ("del".into(), "deleted"),
            ("mod".into(), "modified")
        ]
    );
}
