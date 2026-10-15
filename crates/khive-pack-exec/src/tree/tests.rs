use super::*;

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
fn accepts_regular_executable_and_symlink_modes() {
    let r = digest_hex(b"target");
    let entries = parse_entries(&json!([
        {"path": "a", "ref": r, "mode": 644},
        {"path": "b", "ref": r, "mode": 755},
        {"path": "c", "ref": r, "mode": 120000}
    ]))
    .unwrap();
    assert_eq!(
        entries.iter().map(|entry| entry.mode).collect::<Vec<_>>(),
        vec![644, 755, 120000]
    );
}

#[test]
fn rejects_duplicates_and_modes() {
    let r = digest_hex(b"x");
    let dup = json!([{"path": "a", "ref": r, "mode": 644}, {"path": "a", "ref": r, "mode": 644}]);
    assert!(parse_entries(&dup).is_err());
    for mode in [0, 777, 0o120777, 0o120000, 100644, 100755, 120001, u64::MAX] {
        let entries = json!([{"path": "a", "ref": r, "mode": mode}]);
        assert!(parse_entries(&entries).is_err(), "mode {mode}");
    }
}

#[test]
fn rejects_entries_beneath_files_and_symlinks_in_either_order() {
    let r = digest_hex(b"target");
    for mode in [644, 755, 120000] {
        let mut entries = vec![
            json!({"path": "a", "ref": r, "mode": mode}),
            json!({"path": "a/b", "ref": r, "mode": 644}),
        ];
        for _ in 0..2 {
            assert!(parse_entries(&json!(entries)).is_err(), "mode {mode}");
            entries.reverse();
        }
    }
}

#[test]
fn ordered_conflict_probe_reports_first_descendant_and_ignores_siblings() {
    let r = digest_hex(b"target");
    let entries = json!([
        {"path": "pkg-other", "ref": r, "mode": 644},
        {"path": "pkg/b", "ref": r, "mode": 644},
        {"path": "pkg/a", "ref": r, "mode": 644},
        {"path": "pkg", "ref": r, "mode": 644},
    ]);
    let error = parse_entries(&entries).unwrap_err();
    assert!(matches!(
        error,
        RuntimeError::InvalidInput(ref message)
            if message == "entry \"pkg\" conflicts with entry \"pkg/a\" (file and directory at one path)"
    ));

    let siblings = json!([
        {"path": "pkg-other", "ref": r, "mode": 644},
        {"path": "pkg", "ref": r, "mode": 644},
    ]);
    assert_eq!(parse_entries(&siblings).unwrap().len(), 2);
}

#[test]
fn compressed_prefix_validation_has_linear_counted_work() {
    let mut index = EntryIndex::default();
    let content_ref = digest_hex(b"target");
    let prefix = "a/".repeat(4_096);
    let mut input_bytes = 0;
    for leaf in ["left", "right"] {
        let path = format!("{prefix}{leaf}");
        input_bytes += path.len();
        insert_entry(
            &mut index,
            TreeEntry {
                path,
                content_ref: content_ref.clone(),
                mode: 644,
            },
        )
        .unwrap();
    }
    for sibling in 0..4_096 {
        let path = format!("bulk/{sibling:04}");
        input_bytes += path.len();
        insert_entry(
            &mut index,
            TreeEntry {
                path,
                content_ref: content_ref.clone(),
                mode: 644,
            },
        )
        .unwrap();
    }
    assert_eq!(index.entries.len(), 4_098);
    let counted = index.work.compared_bytes + index.work.child_lookups;
    assert!(
        counted <= 4 * input_bytes,
        "prefix probes examined {counted} units for {input_bytes} input bytes"
    );
    assert_eq!(index.work.ordered_probes, 3 * index.entries.len());
}

#[test]
fn compressed_prefix_index_preserves_boundary_and_lexical_error_order() {
    let r = digest_hex(b"target");
    let entries = json!([
        {"path": "pkg/a/z", "ref": r, "mode": 644},
        {"path": "pkg/a.", "ref": r, "mode": 644},
        {"path": "pkg", "ref": r, "mode": 644},
    ]);
    let error = parse_entries(&entries).unwrap_err();
    assert!(matches!(
        error,
        RuntimeError::InvalidInput(ref message)
            if message == "entry \"pkg\" conflicts with entry \"pkg/a.\" (file and directory at one path)"
    ));

    let neighbors = json!([
        {"path": "éclair", "ref": r, "mode": 644},
        {"path": "éclairer/child", "ref": r, "mode": 644},
    ]);
    assert_eq!(parse_entries(&neighbors).unwrap().len(), 2);
    let children = json!([
        {"path": "éclair", "ref": r, "mode": 644},
        {"path": "éclair/child", "ref": r, "mode": 644},
    ]);
    let error = parse_entries(&children).unwrap_err();
    assert!(matches!(
        error,
        RuntimeError::InvalidInput(ref message)
            if message == "entry \"éclair/child\" conflicts with entry \"éclair\" (file and directory at one path)"
    ));
}

#[test]
fn wire_validation_reports_the_first_invalid_entry() {
    let r = digest_hex(b"target");
    let bad_path = json!([
        {"path": "bad//path", "mode": 644},
        {"path": "later", "ref": r, "mode": "invalid"},
    ]);
    let error = parse_entries(&bad_path).unwrap_err();
    assert!(matches!(
        error,
        RuntimeError::InvalidInput(ref message)
            if message == "entry path \"bad//path\" is not normalized (empty, '.' or '..' component)"
    ));

    let bad_ref = json!([{"path": "bad-ref", "ref": "invalid"}]);
    let error = parse_entries(&bad_ref).unwrap_err();
    assert!(matches!(
        error,
        RuntimeError::InvalidInput(ref message) if message.starts_with("entry \"bad-ref\" ref:")
    ));

    let conflict = json!([
        {"path": "file", "ref": r, "mode": 644},
        {"path": "file/child", "ref": r, "mode": 644},
        {"mode": "invalid"},
    ]);
    let error = parse_entries(&conflict).unwrap_err();
    assert!(matches!(
        error,
        RuntimeError::InvalidInput(ref message)
            if message == "entry \"file/child\" conflicts with entry \"file\" (file and directory at one path)"
    ));
}

#[test]
fn store_preflight_rejects_output_paths_that_tree_get_would_reject() {
    let entry = TreeEntry {
        path: "bad\\name".into(),
        content_ref: digest_hex(b"output"),
        mode: 644,
    };
    let error = validate_entries(&[entry]).unwrap_err().to_string();
    assert!(error.contains("forbidden character"), "{error}");
}

#[test]
fn store_preflight_refuses_a_manifest_larger_than_tree_get_can_read() {
    let entry = TreeEntry {
        path: "p".repeat(MAX_MANIFEST_BYTES as usize),
        content_ref: digest_hex(b"output"),
        mode: 644,
    };
    let error = canonical_bytes(&[entry]).unwrap_err().to_string();
    assert!(error.contains("manifest exceeds"), "{error}");
}

#[test]
fn manifest_scale_disjoint_paths_validate_without_quadratic_scanning() {
    let content_ref = digest_hex(b"output");
    let entries: Vec<TreeEntry> = (0..60_000)
        .map(|index| TreeEntry {
            path: format!("dir-{index:05}/file"),
            content_ref: content_ref.clone(),
            mode: 644,
        })
        .collect();
    let validated = parse_entries(&entries_json(&entries)).unwrap();
    assert_eq!(validated.len(), entries.len());
    assert_eq!(validated.first().unwrap().path, "dir-00000/file");
    assert_eq!(validated.last().unwrap().path, "dir-59999/file");
    assert!(canonical_bytes(&validated).unwrap().len() <= MAX_MANIFEST_BYTES as usize);
}

#[test]
fn canonical_bytes_are_order_independent() {
    let r = digest_hex(b"x");
    let a = vec![
        TreeEntry {
            path: "b".into(),
            content_ref: r.clone(),
            mode: 644,
        },
        TreeEntry {
            path: "a".into(),
            content_ref: r.clone(),
            mode: 755,
        },
    ];
    let mut b = a.clone();
    b.reverse();
    assert_eq!(canonical_bytes(&a).unwrap(), canonical_bytes(&b).unwrap());
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
