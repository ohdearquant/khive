use super::*;
use crate::RuntimeConfig;

fn digest_hex(bytes: &[u8]) -> String {
    khive_types::Hash32::from_blake3(bytes).to_string()
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

// Proposed literal fixture, not a captured baseline receipt. The host baseline
// overlay runs this same assertion against c286a7a0 before the extracted owner.
mod golden_fixture {
    include!("fixtures/manifest_v1.rs");
}

#[tokio::test]
#[serial_test::serial(background_tasks)]
async fn manifest_v1_literal_bytes_and_reference_are_stable() {
    const GOLDEN: &[u8] = golden_fixture::BYTES;
    let dir = tempfile::tempdir().unwrap();
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(dir.path().join("runtime.db")),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let blobs: std::sync::Arc<dyn BlobStore> = std::sync::Arc::new(
        khive_db::stores::blob::FsBlobStore::new(dir.path().join("blobs"), 0).unwrap(),
    );
    rt.install_blob_store(blobs.clone()).unwrap();
    let entries = vec![
        TreeEntry {
            path: "é/link".into(),
            content_ref: "2".repeat(64),
            mode: 120000,
        },
        TreeEntry {
            path: "a\"line\n.txt".into(),
            content_ref: "0".repeat(64),
            mode: 644,
        },
        TreeEntry {
            path: "bin/run".into(),
            content_ref: "1".repeat(64),
            mode: 755,
        },
    ];
    let reference = store(&rt, &entries).await.unwrap();
    let parsed_ref = ContentRef::from_hex(&reference).unwrap();
    let bytes = blobs
        .get_bounded_verified(&parsed_ref, MAX_MANIFEST_BYTES)
        .await
        .unwrap();
    assert_eq!(
        bytes, GOLDEN,
        "v1 field order, escaping, sorting and byte layout"
    );
    // This expectation is derived from fixed literal bytes, never the serializer.
    assert_eq!(reference, digest_hex(GOLDEN));
    println!("manifest_v1_reference={reference}");
    let fixture_ref = blobs.put(GOLDEN.to_vec()).await.unwrap();
    assert_eq!(fixture_ref, parsed_ref);
    let mut loaded = load(&rt, fixture_ref.as_str()).await.unwrap();
    assert_eq!(loaded.len(), 3);
    loaded.reverse();
    assert_eq!(store(&rt, &loaded).await.unwrap(), reference);
}

#[derive(Debug, Default)]
struct CountingPutStore(std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl BlobStore for CountingPutStore {
    async fn put(&self, bytes: Vec<u8>) -> khive_storage::StorageResult<ContentRef> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(ContentRef::from_hex(digest_hex(&bytes)).unwrap())
    }

    async fn get_bounded_verified(
        &self,
        _reference: &ContentRef,
        _max_bytes: u64,
    ) -> khive_storage::StorageResult<Vec<u8>> {
        panic!("store preflight must not read blobs")
    }

    async fn exists(&self, _reference: &ContentRef) -> khive_storage::StorageResult<bool> {
        panic!("store preflight must not inspect blobs")
    }

    async fn size(&self, _reference: &ContentRef) -> khive_storage::StorageResult<Option<u64>> {
        panic!("store preflight must not inspect blob sizes")
    }

    async fn delete(&self, _reference: &ContentRef) -> khive_storage::StorageResult<bool> {
        panic!("store preflight must not delete blobs")
    }
}

#[tokio::test]
async fn store_publishes_at_byte_limit_and_refuses_over_limit_before_put() {
    let dir = tempfile::tempdir().unwrap();
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(dir.path().join("runtime.db")),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let blobs = std::sync::Arc::new(CountingPutStore::default());
    rt.install_blob_store(blobs.clone()).unwrap();
    let mut entry = TreeEntry {
        path: String::new(),
        content_ref: "0".repeat(64),
        mode: 644,
    };
    let overhead = canonical_bytes(&[entry.clone()]).unwrap().len();
    let path_bytes = MAX_MANIFEST_BYTES as usize - overhead;
    // Newlines occupy one input byte but two serialized bytes. Test the JSON
    // budget, including escaping, rather than an input-string length limit.
    entry.path = "\n".repeat(path_bytes / 2);
    if !path_bytes.is_multiple_of(2) {
        entry.path.push('x');
    }
    assert_eq!(
        canonical_bytes(&[entry.clone()]).unwrap().len(),
        MAX_MANIFEST_BYTES as usize
    );
    store(&rt, &[entry.clone()]).await.unwrap();
    assert_eq!(blobs.0.load(std::sync::atomic::Ordering::Relaxed), 1);
    entry.path.push('x');
    let error = store(&rt, &[entry]).await.unwrap_err();
    assert!(
        matches!(error, RuntimeError::InvalidInput(ref text) if text.contains("manifest exceeds"))
    );
    assert_eq!(blobs.0.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[tokio::test]
#[serial_test::serial(background_tasks)]
async fn load_preserves_reference_schema_and_missing_entry_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(dir.path().join("runtime.db")),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let blobs: std::sync::Arc<dyn BlobStore> = std::sync::Arc::new(
        khive_db::stores::blob::FsBlobStore::new(dir.path().join("blobs"), 0).unwrap(),
    );
    rt.install_blob_store(blobs.clone()).unwrap();
    assert!(matches!(
        load(&rt, "invalid").await,
        Err(RuntimeError::InvalidInput(_))
    ));
    let absent = digest_hex(b"absent");
    assert!(matches!(
        load(&rt, &absent).await,
        Err(RuntimeError::NotFound(_))
    ));
    for bytes in [
        b"not JSON".as_slice(),
        br#"{"schema":"khive-tree/v2","entries":[]}"#.as_slice(),
    ] {
        let reference = blobs.put(bytes.to_vec()).await.unwrap();
        assert!(matches!(
            load(&rt, reference.as_str()).await,
            Err(RuntimeError::InvalidInput(_))
        ));
    }
    let entries = vec![TreeEntry {
        path: "missing".into(),
        content_ref: absent,
        mode: 644,
    }];
    let reference = store(&rt, &entries).await.unwrap();
    assert_eq!(load(&rt, &reference).await.unwrap(), entries);
    assert!(matches!(
        verify_blobs(&rt, &entries).await,
        Err(RuntimeError::NotFound(_))
    ));
}

fn file_runtime(dir: &tempfile::TempDir) -> (KhiveRuntime, std::sync::Arc<dyn BlobStore>) {
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(dir.path().join("runtime.db")),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let blobs: std::sync::Arc<dyn BlobStore> = std::sync::Arc::new(
        khive_db::stores::blob::FsBlobStore::new(dir.path().join("blobs"), 0).unwrap(),
    );
    rt.install_blob_store(blobs.clone()).unwrap();
    (rt, blobs)
}

fn owned(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|path| path.to_string()).collect()
}

#[test]
fn canonical_scope_sorts_and_refuses_repeats_nesting_and_bounds() {
    assert_eq!(
        canonical_scope(&owned(&["src/b", "docs"])).unwrap(),
        owned(&["docs", "src/b"])
    );
    // Siblings that share a prefix are not nested.
    assert_eq!(
        canonical_scope(&owned(&["ab", "a-b", "a"])).unwrap(),
        owned(&["a", "a-b", "ab"])
    );
    for bad in [
        Vec::new(),
        owned(&["a", "a"]),
        owned(&["a", "a/b"]),
        // `a` and `a/c` are not adjacent once sorted, and are still nested.
        owned(&["a/c", "a-b", "a"]),
        owned(&["../x"]),
        owned(&["/abs"]),
    ] {
        assert!(
            matches!(canonical_scope(&bad), Err(RuntimeError::InvalidInput(_))),
            "{bad:?}"
        );
    }
    let at_limit: Vec<String> = (0..MAX_SCOPE_PATHS).map(|i| format!("p{i:02}")).collect();
    assert_eq!(canonical_scope(&at_limit).unwrap().len(), MAX_SCOPE_PATHS);
    let over: Vec<String> = (0..=MAX_SCOPE_PATHS).map(|i| format!("p{i:02}")).collect();
    assert!(canonical_scope(&over).is_err());
}

#[tokio::test]
#[serial_test::serial(background_tasks)]
async fn scoped_tree_keeps_its_scope_and_whole_tree_load_refuses_it() {
    let dir = tempfile::tempdir().unwrap();
    let (rt, blobs) = file_runtime(&dir);
    let entries = vec![
        TreeEntry {
            path: "src/b/x".into(),
            content_ref: "1".repeat(64),
            mode: 644,
        },
        TreeEntry {
            path: "docs/a".into(),
            content_ref: "0".repeat(64),
            mode: 644,
        },
    ];
    let scoped = store_tree(&rt, &entries, Some(&owned(&["src/b", "docs"])))
        .await
        .unwrap();
    let whole = store(&rt, &entries).await.unwrap();
    assert_ne!(
        scoped, whole,
        "the same entries scoped and whole are two trees"
    );
    let bytes = blobs
        .get_bounded_verified(&ContentRef::from_hex(&scoped).unwrap(), MAX_MANIFEST_BYTES)
        .await
        .unwrap();
    let expected = format!(
        r#"{{"schema":"khive-tree/scoped-v1","scope":["docs","src/b"],"entries":[{{"path":"docs/a","ref":"{}","mode":644}},{{"path":"src/b/x","ref":"{}","mode":644}}]}}"#,
        "0".repeat(64),
        "1".repeat(64)
    );
    // The stored schema is not the whole-tree schema, so a store that knows only v1 refuses it.
    assert_eq!(String::from_utf8(bytes).unwrap(), expected);
    let loaded = load_tree(&rt, &scoped).await.unwrap();
    assert_eq!(loaded.scope, Some(owned(&["docs", "src/b"])));
    assert_eq!(loaded.entries.len(), 2);
    assert_eq!(
        store_tree(&rt, &loaded.entries, loaded.scope.as_deref())
            .await
            .unwrap(),
        scoped,
        "storing a loaded scoped tree with its scope is the same tree"
    );
    let whole_loaded = load_tree(&rt, &whole).await.unwrap();
    assert_eq!(whole_loaded.scope, None);
    assert_eq!(load(&rt, &whole).await.unwrap(), whole_loaded.entries);
    match load(&rt, &scoped).await {
        Err(RuntimeError::InvalidInput(text)) => {
            assert!(text.contains("scoped to docs, src/b"), "{text}")
        }
        other => panic!("a whole-tree load of a scoped tree answered {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial(background_tasks)]
async fn load_refuses_scope_and_schema_mismatches() {
    let dir = tempfile::tempdir().unwrap();
    let (rt, blobs) = file_runtime(&dir);
    let r = "0".repeat(64);
    let entries = format!(r#"[{{"path":"a/x","ref":"{r}","mode":644}}]"#);
    let cases = [
        (
            "whole schema with a scope",
            format!(r#"{{"schema":"khive-tree/v1","scope":["a"],"entries":{entries}}}"#),
        ),
        (
            "scoped schema without a scope",
            format!(r#"{{"schema":"khive-tree/scoped-v1","entries":{entries}}}"#),
        ),
        (
            "scope out of canonical order",
            format!(r#"{{"schema":"khive-tree/scoped-v1","scope":["b","a"],"entries":{entries}}}"#),
        ),
        (
            "nested scope",
            format!(
                r#"{{"schema":"khive-tree/scoped-v1","scope":["a","a/x"],"entries":{entries}}}"#
            ),
        ),
        (
            "unknown schema",
            format!(r#"{{"schema":"khive-tree/scoped-v2","scope":["a"],"entries":{entries}}}"#),
        ),
    ];
    for (label, bytes) in cases {
        let reference = blobs.put(bytes.into_bytes()).await.unwrap();
        for refused in [
            load_tree(&rt, reference.as_str()).await.map(|_| ()),
            load(&rt, reference.as_str()).await.map(|_| ()),
        ] {
            assert!(
                matches!(refused, Err(RuntimeError::InvalidInput(_))),
                "{label}: {refused:?}"
            );
        }
    }
    // Control: the same scoped bytes in canonical form load.
    let canonical =
        format!(r#"{{"schema":"khive-tree/scoped-v1","scope":["a"],"entries":{entries}}}"#);
    let reference = blobs.put(canonical.into_bytes()).await.unwrap();
    assert_eq!(
        load_tree(&rt, reference.as_str()).await.unwrap().scope,
        Some(owned(&["a"]))
    );
}
