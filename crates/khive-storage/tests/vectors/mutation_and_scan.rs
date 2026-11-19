// ---------------------------------------------------------------------------
// Test cases — orphan_sweep
// ---------------------------------------------------------------------------

#[tokio::test]
async fn orphan_sweep_default_returns_unsupported() {
    let store = TestVectorStore::new();
    let config = OrphanSweepConfig {
        subject_id_allowlist: None,
        namespaces: vec![],
        substrate_kinds: vec![],
        max_delete: 100,
        dry_run: true,
    };
    let result = store.orphan_sweep(&config).await;
    assert!(
        matches!(result, Err(StorageError::Unsupported { .. })),
        "expected Unsupported, got {result:?}"
    );
    assert!(!store.capabilities().supports_orphan_sweep);
}

// ---------------------------------------------------------------------------
// Test cases — update
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_calls_delete_then_insert() {
    let store = TestVectorStore::new();
    let id = Uuid::new_v4();
    let result = store
        .update(
            id,
            SubstrateKind::Entity,
            "ns:test",
            "body",
            vec![vec![0.1, 0.2]],
        )
        .await;
    assert!(result.is_ok());
    assert!(
        store.delete_called.load(Ordering::SeqCst),
        "delete must be called"
    );
    assert!(
        store.insert_called.load(Ordering::SeqCst),
        "insert must be called after delete"
    );
}

#[tokio::test]
async fn update_propagates_delete_failure() {
    let store = TestVectorStore::with_fail_delete();
    let id = Uuid::new_v4();
    let result = store
        .update(
            id,
            SubstrateKind::Entity,
            "ns:test",
            "body",
            vec![vec![0.1, 0.2]],
        )
        .await;
    assert!(result.is_err());
    assert!(
        store.delete_called.load(Ordering::SeqCst),
        "delete must be attempted"
    );
    assert!(
        !store.insert_called.load(Ordering::SeqCst),
        "insert must NOT be called when delete fails"
    );
}

#[tokio::test]
async fn update_propagates_insert_failure() {
    let store = TestVectorStore::with_fail_insert();
    let id = Uuid::new_v4();
    let result = store
        .update(
            id,
            SubstrateKind::Entity,
            "ns:test",
            "body",
            vec![vec![0.1, 0.2]],
        )
        .await;
    assert!(result.is_err());
    assert!(
        store.insert_called.load(Ordering::SeqCst),
        "insert must be attempted"
    );
}

// ---------------------------------------------------------------------------
// Test cases — VectorMetadataFilter helpers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn vector_metadata_filter_is_empty_with_property_filters() {
    let empty = VectorMetadataFilter::default();
    assert!(empty.is_empty());

    let with_ns = VectorMetadataFilter {
        namespaces: vec!["ns".into()],
        ..Default::default()
    };
    assert!(!with_ns.is_empty());

    let with_prop = VectorMetadataFilter {
        property_filters: vec![PropertyFilter {
            key: "k".into(),
            op: PropertyOp::Eq,
            value: serde_json::Value::Bool(true),
        }],
        ..Default::default()
    };
    assert!(!with_prop.is_empty());
}

#[tokio::test]
async fn vector_scan_default_is_explicitly_unsupported_through_dyn_store() {
    let store = TestVectorStore::new();
    let capability: &dyn VectorStore = &store;
    let error = capability
        .scan_vectors(khive_storage::VectorScanRequest {
            namespace: "ns:test".into(),
            embedding_model: "test".into(),
            field: "body".into(),
            kind: None,
            after: None,
            limit: std::num::NonZeroU32::new(1).unwrap(),
        })
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::Unsupported {
        capability: StorageCapability::Vectors, operation, ..
    } if operation == "scan_vectors"));
    assert!(!store.search_called.load(Ordering::SeqCst));
    assert!(!store.insert_called.load(Ordering::SeqCst));
    assert!(!store.delete_called.load(Ordering::SeqCst));
}

#[test]
fn vector_scan_request_wire_rejects_invalid_cursor_limit_and_kind() {
    use khive_storage::VectorScanRequest;
    let valid = serde_json::json!({
        "namespace": "", "embedding_model": "m'_%", "field": "body",
        "kind": "note", "after": "00000000-0000-0000-0000-00000000000a", "limit": 1
    });
    let request: VectorScanRequest = serde_json::from_value(valid.clone()).unwrap();
    assert_eq!(request.namespace, "");
    assert_eq!(request.embedding_model, "m'_%");
    assert_eq!(request.kind, Some(SubstrateKind::Note));
    assert_eq!(request.after, Some(Uuid::from_u128(10)));
    assert_eq!(serde_json::to_value(&request).unwrap(), valid);
    for (field, value) in [
        ("after", serde_json::json!("not-a-uuid")),
        ("after", serde_json::json!(2)),
        ("limit", serde_json::json!(0)),
        ("limit", serde_json::json!(-1)),
        ("limit", serde_json::json!(4_294_967_296_u64)),
        ("limit", serde_json::json!(1.5)),
        ("limit", serde_json::json!(null)),
        ("kind", serde_json::json!("concept")),
        ("namespace", serde_json::json!(null)),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        assert!(
            serde_json::from_value::<VectorScanRequest>(invalid).is_err(),
            "{field}"
        );
    }
    for field in ["namespace", "embedding_model", "field", "limit"] {
        let mut invalid = valid.clone();
        invalid.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<VectorScanRequest>(invalid).is_err(),
            "{field}"
        );
    }
    let mut optional = valid;
    optional["kind"] = serde_json::Value::Null;
    optional["after"] = serde_json::Value::Null;
    optional["limit"] = serde_json::json!(u32::MAX);
    let request: VectorScanRequest = serde_json::from_value(optional).unwrap();
    assert!(request.kind.is_none());
    assert!(request.after.is_none());
    assert_eq!(request.limit.get(), u32::MAX);
}

#[test]
fn vector_scan_page_wire_preserves_multiple_vectors_and_terminal_cursor() {
    use khive_storage::{VectorScanEntry, VectorScanPage};
    let page = VectorScanPage {
        items: vec![VectorScanEntry {
            subject_id: Uuid::from_u128(7),
            vectors: vec![vec![1.0, -0.25], vec![0.125, 2.0]],
        }],
        next_after: Some(Uuid::from_u128(7)),
    };
    let value = serde_json::to_value(&page).unwrap();
    assert_eq!(
        value["items"][0]["vectors"],
        serde_json::json!([[1.0, -0.25], [0.125, 2.0]])
    );
    assert_eq!(
        serde_json::from_value::<VectorScanPage>(value).unwrap(),
        page
    );
    let empty = VectorScanPage {
        items: vec![],
        next_after: None,
    };
    assert_eq!(
        serde_json::to_value(empty).unwrap(),
        serde_json::json!({"items": [], "next_after": null})
    );
}
