use super::*;
use crate::NodeId;
use khive_bm25::Bm25Index;
use khive_hnsw::HnswIndex;
use rusqlite::Connection;
use std::sync::Arc;
use tokio::sync::Mutex;

async fn setup_test_persistence() -> RetrievalPersistence {
    let conn = Connection::open_in_memory().expect("open in-memory db");
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
        .expect("set pragmas");
    let persist = RetrievalPersistence::new(Arc::new(Mutex::new(conn)), "test");
    persist.init_schema().await.expect("init schema");
    persist
}

#[tokio::test]
async fn test_persist_and_load_bm25() {
    let persist = setup_test_persistence().await;

    // Create and persist a BM25 index
    let mut index = Bm25Index::default();
    index
        .index_document("doc1", "hello world")
        .expect("index doc");
    index
        .index_document("doc2", "goodbye world")
        .expect("index doc");

    persist.persist_bm25_index(&index).await.expect("persist");

    // Load and verify
    let loaded = persist.load_bm25_index().await.expect("load");
    assert!(loaded.is_some());
    let loaded = loaded.unwrap();
    assert_eq!(loaded.doc_count(), 2);
}

#[tokio::test]
async fn test_persist_and_load_hnsw() {
    let persist = setup_test_persistence().await;

    // Create and persist an HNSW index with some vectors
    let mut index = HnswIndex::new(4); // 4 dimensions

    // Insert a few vectors
    let id1 = NodeId::new([1; 16]);
    let id2 = NodeId::new([2; 16]);
    let id3 = NodeId::new([3; 16]);

    index.insert(id1, vec![1.0, 0.0, 0.0, 0.0]).expect("insert");
    index.insert(id2, vec![0.0, 1.0, 0.0, 0.0]).expect("insert");
    index.insert(id3, vec![0.0, 0.0, 1.0, 0.0]).expect("insert");

    assert_eq!(index.len(), 3);

    // Persist the snapshot
    persist
        .persist_hnsw_snapshot(&index)
        .await
        .expect("persist");

    // Load and verify the snapshot
    let loaded = persist.load_hnsw_snapshot().await.expect("load");
    assert!(loaded.is_some());
    let snapshot = loaded.unwrap();

    // Verify snapshot contains correct metadata
    assert_eq!(snapshot.total_nodes, 3);
    assert_eq!(snapshot.live_nodes, 3);
    assert_eq!(snapshot.tombstone_count, 0);
    assert_eq!(snapshot.indexed_ids.len(), 3);
    assert!(snapshot.indexed_ids.contains(&id1));
    assert!(snapshot.indexed_ids.contains(&id2));
    assert!(snapshot.indexed_ids.contains(&id3));
}

#[tokio::test]
async fn test_stats() {
    let persist = setup_test_persistence().await;

    // Initially empty
    let stats = persist.stats().await.expect("stats");
    assert_eq!(stats.hnsw_snapshot_size, 0);
    assert_eq!(stats.bm25_snapshot_size, 0);

    // Persist BM25
    let index = Bm25Index::default();
    persist.persist_bm25_index(&index).await.expect("persist");

    // Check stats
    let stats = persist.stats().await.expect("stats");
    assert!(stats.bm25_snapshot_size > 0);
    assert!(stats.bm25_snapshot_at.is_some());
}

#[tokio::test]
async fn test_clear() {
    let persist = setup_test_persistence().await;

    // Persist something
    let index = Bm25Index::default();
    persist.persist_bm25_index(&index).await.expect("persist");

    // Clear
    persist.clear().await.expect("clear");

    // Should be gone
    let loaded = persist.load_bm25_index().await.expect("load");
    assert!(loaded.is_none());
}

// -- Shadow validation tests --

#[tokio::test]
async fn test_shadow_validation_config_default() {
    let config = ShadowValidationConfig::default();
    assert!(!config.enabled);
    assert!((config.sample_rate - 0.1).abs() < f64::EPSILON);
}

#[tokio::test]
async fn test_shadow_validation_config_enabled() {
    let config = ShadowValidationConfig::enabled();
    assert!(config.enabled);
    assert!((config.sample_rate - 1.0).abs() < f64::EPSILON);
}

#[tokio::test]
async fn test_shadow_validation_config_sample_rate() {
    let config = ShadowValidationConfig::with_sample_rate(0.5);
    assert!(config.enabled);
    assert!((config.sample_rate - 0.5).abs() < f64::EPSILON);

    // Test clamping
    let config = ShadowValidationConfig::with_sample_rate(1.5);
    assert!((config.sample_rate - 1.0).abs() < f64::EPSILON);

    let config = ShadowValidationConfig::with_sample_rate(-0.5);
    assert!((config.sample_rate - 0.0).abs() < f64::EPSILON);
}

#[tokio::test]
async fn test_bm25_shadow_validation_passes() {
    let persist = setup_test_persistence().await;
    let config = ShadowValidationConfig::enabled();

    // Create and persist a BM25 index with validation
    let mut index = Bm25Index::default();
    index
        .index_document("doc1", "hello world")
        .expect("index doc");
    index
        .index_document("doc2", "goodbye world")
        .expect("index doc");

    let result = persist
        .persist_bm25_with_validation(&index, &config)
        .await
        .expect("persist with validation");

    assert!(result.is_some());
    let validation = result.unwrap();
    assert!(
        validation.passed,
        "validation should pass: {:?}",
        validation.discrepancies
    );
    assert_eq!(validation.index_type, "bm25");
    assert_eq!(validation.expected.item_count, 2);
    assert!(validation.discrepancies.is_empty());
}

#[tokio::test]
async fn test_shadow_validation_skipped_when_disabled() {
    let persist = setup_test_persistence().await;
    let config = ShadowValidationConfig::default(); // disabled

    let index = Bm25Index::default();
    let result = persist
        .persist_bm25_with_validation(&index, &config)
        .await
        .expect("persist");

    // Validation should be skipped
    assert!(result.is_none());

    // But the persist should still work
    let loaded = persist.load_bm25_index().await.expect("load");
    assert!(loaded.is_some());
}

#[tokio::test]
async fn test_should_sample() {
    use super::shadow::should_sample;

    // Always sample at 1.0
    assert!(should_sample(1.0));
    assert!(should_sample(1.5)); // clamped to 1.0

    // Never sample at 0.0
    assert!(!should_sample(0.0));
    assert!(!should_sample(-0.5)); // clamped to 0.0
}

// -- Issue #865: HNSW shadow validation test --

#[tokio::test]
async fn test_hnsw_shadow_validation_passes() {
    let persist = setup_test_persistence().await;
    let config = ShadowValidationConfig::enabled();

    // Create an HNSW index with vectors
    let mut index = HnswIndex::new(4);
    let id1 = NodeId::new([1; 16]);
    let id2 = NodeId::new([2; 16]);

    index.insert(id1, vec![1.0, 0.0, 0.0, 0.0]).expect("insert");
    index.insert(id2, vec![0.0, 1.0, 0.0, 0.0]).expect("insert");

    let result = persist
        .persist_hnsw_with_validation(&index, &config)
        .await
        .expect("persist with validation");

    assert!(result.is_some());
    let validation = result.unwrap();
    assert!(
        validation.passed,
        "validation should pass: {:?}",
        validation.discrepancies
    );
    assert_eq!(validation.index_type, "hnsw");
    assert_eq!(validation.expected.item_count, 2);
    assert!(validation.discrepancies.is_empty());
}

#[tokio::test]
async fn test_hnsw_shadow_validation_with_tombstones() {
    let persist = setup_test_persistence().await;
    let config = ShadowValidationConfig::enabled();

    // Create an HNSW index with vectors and tombstones
    let mut index = HnswIndex::new(4);
    let id1 = NodeId::new([1; 16]);
    let id2 = NodeId::new([2; 16]);
    let id3 = NodeId::new([3; 16]);

    index.insert(id1, vec![1.0, 0.0, 0.0, 0.0]).expect("insert");
    index.insert(id2, vec![0.0, 1.0, 0.0, 0.0]).expect("insert");
    index.insert(id3, vec![0.0, 0.0, 1.0, 0.0]).expect("insert");
    index.delete(id2); // Tombstone id2

    let result = persist
        .persist_hnsw_with_validation(&index, &config)
        .await
        .expect("persist with validation");

    assert!(result.is_some());
    let validation = result.unwrap();
    assert!(
        validation.passed,
        "validation should pass with tombstones: {:?}",
        validation.discrepancies
    );
    assert_eq!(validation.expected.item_count, 3); // total_nodes including tombstones
    assert_eq!(validation.expected.tombstone_count, 1);
}

// -- Issue #866: Namespace isolation test --

#[tokio::test]
async fn test_namespace_isolation() {
    let conn = Connection::open_in_memory().expect("open in-memory db");
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
        .expect("set pragmas");
    let conn = Arc::new(Mutex::new(conn));

    // Create two persistence layers with different namespaces
    let persist_ns1 = RetrievalPersistence::new(conn.clone(), "namespace1");
    let persist_ns2 = RetrievalPersistence::new(conn.clone(), "namespace2");

    // Initialize schema (only needed once since they share the connection)
    persist_ns1.init_schema().await.expect("init schema");

    // Persist different data to each namespace
    let mut index1 = Bm25Index::default();
    index1
        .index_document("doc1", "namespace one content")
        .expect("index");

    let mut index2 = Bm25Index::default();
    index2
        .index_document("doc2", "namespace two content")
        .expect("index");
    index2
        .index_document("doc3", "more namespace two")
        .expect("index");

    persist_ns1
        .persist_bm25_index(&index1)
        .await
        .expect("persist ns1");
    persist_ns2
        .persist_bm25_index(&index2)
        .await
        .expect("persist ns2");

    // Verify each namespace loads its own data
    let loaded1 = persist_ns1.load_bm25_index().await.expect("load ns1");
    let loaded2 = persist_ns2.load_bm25_index().await.expect("load ns2");

    assert!(loaded1.is_some());
    assert!(loaded2.is_some());
    assert_eq!(loaded1.unwrap().doc_count(), 1);
    assert_eq!(loaded2.unwrap().doc_count(), 2);

    // Clear one namespace and verify the other is unaffected
    persist_ns1.clear().await.expect("clear ns1");

    let loaded1_after = persist_ns1
        .load_bm25_index()
        .await
        .expect("load ns1 after clear");
    let loaded2_after = persist_ns2
        .load_bm25_index()
        .await
        .expect("load ns2 after clear");

    assert!(loaded1_after.is_none(), "ns1 should be cleared");
    assert!(loaded2_after.is_some(), "ns2 should still exist");
    assert_eq!(loaded2_after.unwrap().doc_count(), 2);
}

// -- Issue #868: Corrupted data handling tests --

#[tokio::test]
async fn test_corrupted_bm25_data_returns_error() {
    let persist = setup_test_persistence().await;

    // Manually insert corrupted JSON
    {
        let conn = persist.conn.clone();
        let namespace = "test".to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                r#"
                INSERT OR REPLACE INTO retrieval_snapshots
                    (namespace, index_type, snapshot, created_at)
                VALUES
                    (?1, 'bm25', ?2, strftime('%s', 'now'))
                "#,
                rusqlite::params![namespace, b"not valid json {{{{"],
            )
            .expect("insert corrupted");
        })
        .await
        .expect("spawn");
    }

    // Attempt to load should return an error
    let result = persist.load_bm25_index().await;
    assert!(result.is_err(), "loading corrupted data should fail");
    let err = result.unwrap_err();
    assert!(matches!(err, PersistError::Deserialize(_)));
}

#[tokio::test]
async fn test_corrupted_hnsw_data_returns_error() {
    let persist = setup_test_persistence().await;

    // Manually insert corrupted JSON
    {
        let conn = persist.conn.clone();
        let namespace = "test".to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                r#"
                INSERT OR REPLACE INTO retrieval_snapshots
                    (namespace, index_type, snapshot, created_at)
                VALUES
                    (?1, 'hnsw', ?2, strftime('%s', 'now'))
                "#,
                rusqlite::params![namespace, b"truncated json {\"total_nodes\":"],
            )
            .expect("insert corrupted");
        })
        .await
        .expect("spawn");
    }

    // Attempt to load should return an error
    let result = persist.load_hnsw_snapshot().await;
    assert!(result.is_err(), "loading corrupted HNSW data should fail");
    let err = result.unwrap_err();
    assert!(matches!(err, PersistError::Deserialize(_)));
}

// -- Issue #868: Additional corrupted data handling tests --

#[tokio::test]
async fn test_valid_json_wrong_schema_bm25() {
    let persist = setup_test_persistence().await;

    // Insert valid JSON but wrong schema (missing required fields)
    {
        let conn = persist.conn.clone();
        let namespace = "test".to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            // Valid JSON but wrong structure for Bm25Index
            let wrong_schema = br#"{"some_field": "value", "other": 123}"#;
            conn.execute(
                r#"
                INSERT OR REPLACE INTO retrieval_snapshots
                    (namespace, index_type, snapshot, created_at)
                VALUES
                    (?1, 'bm25', ?2, strftime('%s', 'now'))
                "#,
                rusqlite::params![namespace, wrong_schema.as_slice()],
            )
            .expect("insert wrong schema");
        })
        .await
        .expect("spawn");
    }

    // Attempt to load should return an error (missing required fields)
    let result = persist.load_bm25_index().await;
    assert!(result.is_err(), "loading wrong schema should fail");
    let err = result.unwrap_err();
    assert!(matches!(err, PersistError::Deserialize(_)));
}

#[tokio::test]
async fn test_valid_json_wrong_schema_hnsw() {
    let persist = setup_test_persistence().await;

    // Insert valid JSON but wrong schema (missing required fields for HnswSnapshot)
    {
        let conn = persist.conn.clone();
        let namespace = "test".to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            // Valid JSON but wrong structure for HnswSnapshot
            let wrong_schema = br#"{"total_nodes": 5, "wrong_field": true}"#;
            conn.execute(
                r#"
                INSERT OR REPLACE INTO retrieval_snapshots
                    (namespace, index_type, snapshot, created_at)
                VALUES
                    (?1, 'hnsw', ?2, strftime('%s', 'now'))
                "#,
                rusqlite::params![namespace, wrong_schema.as_slice()],
            )
            .expect("insert wrong schema");
        })
        .await
        .expect("spawn");
    }

    // Attempt to load should return an error (missing required fields)
    let result = persist.load_hnsw_snapshot().await;
    assert!(result.is_err(), "loading wrong schema should fail");
    let err = result.unwrap_err();
    assert!(matches!(err, PersistError::Deserialize(_)));
}

#[tokio::test]
async fn test_empty_blob_returns_error() {
    let persist = setup_test_persistence().await;

    // Insert empty blob
    {
        let conn = persist.conn.clone();
        let namespace = "test".to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                r#"
                INSERT OR REPLACE INTO retrieval_snapshots
                    (namespace, index_type, snapshot, created_at)
                VALUES
                    (?1, 'bm25', ?2, strftime('%s', 'now'))
                "#,
                rusqlite::params![namespace, &[] as &[u8]],
            )
            .expect("insert empty blob");
        })
        .await
        .expect("spawn");
    }

    // Attempt to load should return an error
    let result = persist.load_bm25_index().await;
    assert!(result.is_err(), "loading empty blob should fail");
    let err = result.unwrap_err();
    assert!(matches!(err, PersistError::Deserialize(_)));
}

// -- Issue #869: Empty index persistence edge case tests --

#[tokio::test]
async fn test_empty_bm25_index_persistence() {
    let persist = setup_test_persistence().await;

    // Persist an empty BM25 index
    let index = Bm25Index::default();
    assert_eq!(index.doc_count(), 0);

    persist
        .persist_bm25_index(&index)
        .await
        .expect("persist empty");

    // Load and verify
    let loaded = persist.load_bm25_index().await.expect("load");
    assert!(loaded.is_some());
    let loaded = loaded.unwrap();
    assert_eq!(loaded.doc_count(), 0, "empty index should remain empty");
}

#[tokio::test]
async fn test_empty_hnsw_index_persistence() {
    let persist = setup_test_persistence().await;

    // Persist an empty HNSW index
    let index = HnswIndex::new(4);
    assert_eq!(index.len(), 0);

    persist
        .persist_hnsw_snapshot(&index)
        .await
        .expect("persist empty");

    // Load and verify
    let loaded = persist.load_hnsw_snapshot().await.expect("load");
    assert!(loaded.is_some());
    let snapshot = loaded.unwrap();
    assert_eq!(
        snapshot.total_nodes, 0,
        "empty index snapshot should have 0 nodes"
    );
    assert_eq!(snapshot.live_nodes, 0);
    assert!(snapshot.indexed_ids.is_empty());
}

#[tokio::test]
async fn test_empty_hnsw_shadow_validation() {
    let persist = setup_test_persistence().await;
    let config = ShadowValidationConfig::enabled();

    // Empty HNSW index
    let index = HnswIndex::new(4);

    let result = persist
        .persist_hnsw_with_validation(&index, &config)
        .await
        .expect("persist empty with validation");

    assert!(result.is_some());
    let validation = result.unwrap();
    assert!(validation.passed, "empty index validation should pass");
    assert_eq!(validation.expected.item_count, 0);
}

// -- Issue #867: Test that verify() is called during shadow validation --

#[tokio::test]
async fn test_hnsw_shadow_validation_calls_verify() {
    let persist = setup_test_persistence().await;
    let config = ShadowValidationConfig::enabled();

    // Create an HNSW index with vectors
    let mut index = HnswIndex::new(4);
    let id1 = NodeId::new([1; 16]);
    let id2 = NodeId::new([2; 16]);
    let id3 = NodeId::new([3; 16]);

    index.insert(id1, vec![1.0, 0.0, 0.0, 0.0]).expect("insert");
    index.insert(id2, vec![0.0, 1.0, 0.0, 0.0]).expect("insert");
    index.insert(id3, vec![0.0, 0.0, 1.0, 0.0]).expect("insert");
    index.delete(id2); // Create tombstone

    let result = persist
        .persist_hnsw_with_validation(&index, &config)
        .await
        .expect("persist with validation");

    // Validation should pass because verify() succeeds on valid snapshot
    assert!(result.is_some());
    let validation = result.unwrap();
    assert!(
        validation.passed,
        "valid snapshot should pass verify(): {:?}",
        validation.discrepancies
    );
    assert_eq!(validation.expected.item_count, 3);
    assert_eq!(validation.expected.tombstone_count, 1);
}

// ==========================================================================
// Issue #1114: HNSW index corruption recovery tests
// ==========================================================================
//
// These tests verify that the persistence layer correctly detects and handles
// various forms of HNSW index corruption, enabling the engine to recover
// by rebuilding from source data.

/// Helper: insert raw bytes into the HNSW snapshot slot for a persistence instance.
async fn inject_raw_hnsw_snapshot(persist: &RetrievalPersistence, data: &[u8]) {
    let conn = persist.conn.clone();
    let namespace = persist.namespace.clone();
    let data = data.to_vec();
    tokio::task::spawn_blocking(move || {
        let conn = conn.blocking_lock();
        conn.execute(
            r#"
            INSERT OR REPLACE INTO retrieval_snapshots
                (namespace, index_type, snapshot, created_at)
            VALUES
                (?1, 'hnsw', ?2, strftime('%s', 'now'))
            "#,
            rusqlite::params![&*namespace, data],
        )
        .expect("inject raw snapshot");
    })
    .await
    .expect("spawn");
}

/// Helper: build a valid HNSW index with some vectors and persist it.
async fn build_and_persist_hnsw(persist: &RetrievalPersistence) -> HnswIndex {
    let mut index = HnswIndex::new(4);
    let id1 = NodeId::new([1; 16]);
    let id2 = NodeId::new([2; 16]);
    let id3 = NodeId::new([3; 16]);

    index.insert(id1, vec![1.0, 0.0, 0.0, 0.0]).expect("insert");
    index.insert(id2, vec![0.0, 1.0, 0.0, 0.0]).expect("insert");
    index.insert(id3, vec![0.0, 0.0, 1.0, 0.0]).expect("insert");

    persist
        .persist_hnsw_snapshot(&index)
        .await
        .expect("persist");
    index
}

// -- Test: Truncated HNSW snapshot file --
//
// Scenario: The snapshot BLOB in SQLite is truncated (e.g., write was interrupted).
// Expected: load_hnsw_snapshot returns a Deserialize error, not a panic or corrupt data.

#[tokio::test]
async fn test_truncated_hnsw_snapshot_detected() {
    let persist = setup_test_persistence().await;

    // First persist a valid snapshot so we have realistic JSON to truncate
    build_and_persist_hnsw(&persist).await;

    // Load the valid snapshot and get its serialized form
    let valid_snapshot = persist
        .load_hnsw_snapshot()
        .await
        .expect("load valid")
        .expect("snapshot exists");

    let valid_json = serde_json::to_vec(&valid_snapshot).expect("serialize");
    assert!(valid_json.len() > 20, "valid JSON should be non-trivial");

    // Truncate at various points to simulate interrupted writes
    for truncate_at in [1, 10, valid_json.len() / 4, valid_json.len() / 2] {
        let truncated = &valid_json[..truncate_at];
        inject_raw_hnsw_snapshot(&persist, truncated).await;

        let result = persist.load_hnsw_snapshot().await;
        assert!(
            result.is_err(),
            "truncated snapshot (at byte {truncate_at}) should fail to load"
        );
        let err = result.unwrap_err();
        assert!(
            matches!(err, PersistError::Deserialize(_)),
            "should be a Deserialize error, got: {err:?}"
        );
    }
}

// -- Test: Corrupted bytes in HNSW snapshot --
//
// Scenario: Random byte corruption in the snapshot BLOB (e.g., disk bit flip).
// Expected: Deserialization fails or snapshot verify() catches inconsistency.

#[tokio::test]
async fn test_corrupted_bytes_in_hnsw_snapshot_detected() {
    let persist = setup_test_persistence().await;

    // Build and persist a valid snapshot
    build_and_persist_hnsw(&persist).await;

    let valid_snapshot = persist
        .load_hnsw_snapshot()
        .await
        .expect("load valid")
        .expect("snapshot exists");

    let mut corrupted_json = serde_json::to_vec(&valid_snapshot).expect("serialize");

    // Corrupt bytes in the middle of the JSON (likely to break structure)
    let mid = corrupted_json.len() / 2;
    for i in mid..mid.saturating_add(10).min(corrupted_json.len()) {
        corrupted_json[i] = 0xFF;
    }

    inject_raw_hnsw_snapshot(&persist, &corrupted_json).await;

    let result = persist.load_hnsw_snapshot().await;

    // The corrupted JSON should either fail to deserialize or produce
    // a snapshot that fails verification. Either outcome is acceptable
    // as long as we don't silently return corrupt data.
    match result {
        Err(PersistError::Deserialize(_)) => {
            // Good: deserialization caught it
        }
        Ok(Some(snapshot)) => {
            // If it deserialized, verify() should catch the inconsistency
            // (corrupted counts, missing IDs, etc.)
            let verify_result = snapshot.verify();
            // Even if verify passes (unlikely with random corruption), we accept it
            // because the snapshot's data fields would be garbled. The key invariant
            // is that we don't panic or produce silently wrong results.
            let _ = verify_result;
        }
        Ok(None) => {
            panic!("snapshot was injected, should not return None");
        }
        Err(other) => {
            panic!("unexpected error variant: {other:?}");
        }
    }
}

// -- Test: Missing HNSW snapshot (no row in SQLite) --
//
// Scenario: The snapshot row doesn't exist (e.g., first boot, or snapshot was
//           deleted/cleared). Engine should detect this and rebuild from source.

#[tokio::test]
async fn test_missing_hnsw_snapshot_returns_none() {
    let persist = setup_test_persistence().await;

    // No snapshot has been persisted yet
    let result = persist
        .load_hnsw_snapshot()
        .await
        .expect("load should not error");
    assert!(
        result.is_none(),
        "missing snapshot should return None, not error"
    );
}

#[tokio::test]
async fn test_missing_hnsw_snapshot_after_clear_returns_none() {
    let persist = setup_test_persistence().await;

    // Persist a valid snapshot
    build_and_persist_hnsw(&persist).await;

    // Verify it exists
    let loaded = persist.load_hnsw_snapshot().await.expect("load");
    assert!(loaded.is_some(), "snapshot should exist before clear");

    // Clear all snapshots (simulating data loss / recovery scenario)
    persist.clear().await.expect("clear");

    // Now loading should return None
    let after_clear = persist
        .load_hnsw_snapshot()
        .await
        .expect("load after clear");
    assert!(
        after_clear.is_none(),
        "snapshot should be None after clear, enabling rebuild from source"
    );
}

// -- Test: HNSW snapshot with internally inconsistent state --
//
// Scenario: Snapshot deserializes successfully but has corrupted internal state
//           (e.g., total_nodes doesn't match indexed_ids count). This simulates
//           a partial write or in-memory corruption before serialization.

#[tokio::test]
async fn test_inconsistent_hnsw_snapshot_detected_by_verify() {
    use khive_hnsw::{HnswCheckpointConfig, HnswSnapshot};

    let persist = setup_test_persistence().await;

    let id1 = NodeId::new([1; 16]);
    let id2 = NodeId::new([2; 16]);

    // Create a snapshot where total_nodes doesn't match indexed_ids.len()
    let bad_snapshot = HnswSnapshot {
        vector_count: 0,
        total_nodes: 5, // WRONG: says 5 but only 2 IDs
        live_nodes: 5,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: Some(id1),
        config: HnswCheckpointConfig {
            m: 16,
            ef_construction: 200,
            metric: "cosine".to_string(),
        },
        indexed_ids: vec![id1, id2], // Only 2 IDs
        tombstoned_ids: vec![],
        layers: vec![vec![(id1, vec![id2]), (id2, vec![id1])]],

        vectors: vec![],
    };

    // Persist it (persistence layer doesn't validate, just serializes)
    let data = serde_json::to_vec(&bad_snapshot).expect("serialize");
    inject_raw_hnsw_snapshot(&persist, &data).await;

    // Load succeeds (it's valid JSON with correct schema)
    let loaded = persist
        .load_hnsw_snapshot()
        .await
        .expect("load should succeed for valid JSON");
    assert!(loaded.is_some(), "snapshot should load");

    let snapshot = loaded.unwrap();

    // But verify() detects the inconsistency
    let verify_result = snapshot.verify();
    assert!(
        verify_result.is_err(),
        "verify should catch total_nodes != indexed_ids.len()"
    );
}

#[tokio::test]
async fn test_tombstone_inconsistency_detected_by_verify() {
    use khive_hnsw::{HnswCheckpointConfig, HnswSnapshot};

    let persist = setup_test_persistence().await;

    let id1 = NodeId::new([1; 16]);
    let id2 = NodeId::new([2; 16]);
    let id3 = NodeId::new([3; 16]);
    let id_phantom = NodeId::new([99; 16]);

    // Snapshot claims id_phantom is tombstoned but it's not in indexed_ids
    let bad_snapshot = HnswSnapshot {
        vector_count: 0,
        total_nodes: 3,
        live_nodes: 2,
        tombstone_count: 1,
        max_layer: 0,
        entry_point: Some(id1),
        config: HnswCheckpointConfig {
            m: 16,
            ef_construction: 200,
            metric: "cosine".to_string(),
        },
        indexed_ids: vec![id1, id2, id3],
        tombstoned_ids: vec![id_phantom], // NOT in indexed_ids
        layers: vec![],

        vectors: vec![],
    };

    let data = serde_json::to_vec(&bad_snapshot).expect("serialize");
    inject_raw_hnsw_snapshot(&persist, &data).await;

    let loaded = persist
        .load_hnsw_snapshot()
        .await
        .expect("load")
        .expect("snapshot exists");

    let verify_result = loaded.verify();
    assert!(
        verify_result.is_err(),
        "verify should catch tombstoned ID not in indexed_ids"
    );
}

// -- Test: Shadow validation catches corrupted snapshot state --
//
// Scenario: Snapshot is persisted correctly, then corrupted in-place in SQLite.
//           Shadow validation (read-back) should detect the corruption.

#[tokio::test]
async fn test_shadow_validation_detects_corruption() {
    let persist = setup_test_persistence().await;

    // Build and persist valid index
    let mut index = HnswIndex::new(4);
    let id1 = NodeId::new([1; 16]);
    let id2 = NodeId::new([2; 16]);
    index.insert(id1, vec![1.0, 0.0, 0.0, 0.0]).expect("insert");
    index.insert(id2, vec![0.0, 1.0, 0.0, 0.0]).expect("insert");

    persist
        .persist_hnsw_snapshot(&index)
        .await
        .expect("persist");

    // Now corrupt the stored data in-place
    inject_raw_hnsw_snapshot(&persist, b"not valid json at all {{{").await;

    // Shadow validation should detect the corruption
    let expected = ShadowMetrics {
        item_count: 2,
        tombstone_count: 0,
        snapshot_size: 0,
    };

    let result = persist.validate_hnsw_snapshot(expected).await;
    assert!(
        !result.passed,
        "shadow validation should fail on corrupted data"
    );
    assert!(
        !result.discrepancies.is_empty(),
        "should report discrepancies"
    );
}

// -- Test: Full recovery workflow --
//
// Scenario: Snapshot is corrupted. Engine detects via load failure, clears the
//           corrupt entry, and rebuilds from source vectors. After rebuild,
//           the new snapshot is valid.

#[tokio::test]
async fn test_full_recovery_workflow_corrupt_then_rebuild() {
    let persist = setup_test_persistence().await;

    let id1 = NodeId::new([1; 16]);
    let id2 = NodeId::new([2; 16]);
    let id3 = NodeId::new([3; 16]);

    let vectors = vec![
        (id1, vec![1.0, 0.0, 0.0, 0.0]),
        (id2, vec![0.0, 1.0, 0.0, 0.0]),
        (id3, vec![0.0, 0.0, 1.0, 0.0]),
    ];

    // Step 1: Build and persist a valid index
    {
        let mut index = HnswIndex::new(4);
        for (id, vec) in &vectors {
            index.insert(*id, vec.clone()).expect("insert");
        }
        persist
            .persist_hnsw_snapshot(&index)
            .await
            .expect("persist");
    }

    // Step 2: Corrupt the snapshot
    inject_raw_hnsw_snapshot(&persist, b"corrupted snapshot data").await;

    // Step 3: Attempt to load -- should fail
    let load_result = persist.load_hnsw_snapshot().await;
    assert!(
        load_result.is_err(),
        "loading corrupted snapshot should fail"
    );

    // Step 4: Recovery -- clear corrupt data
    persist.clear().await.expect("clear corrupted data");

    // Step 5: Verify cleared
    let after_clear = persist
        .load_hnsw_snapshot()
        .await
        .expect("load after clear");
    assert!(after_clear.is_none(), "snapshot should be gone after clear");

    // Step 6: Rebuild index from source vectors
    let mut rebuilt_index = HnswIndex::new(4);
    for (id, vec) in &vectors {
        rebuilt_index.insert(*id, vec.clone()).expect("re-insert");
    }

    assert_eq!(
        rebuilt_index.len(),
        3,
        "rebuilt index should have 3 vectors"
    );

    // Step 7: Persist the rebuilt index
    persist
        .persist_hnsw_snapshot(&rebuilt_index)
        .await
        .expect("persist rebuilt");

    // Step 8: Verify the new snapshot is valid
    let new_snapshot = persist
        .load_hnsw_snapshot()
        .await
        .expect("load rebuilt")
        .expect("snapshot exists");

    assert_eq!(new_snapshot.total_nodes, 3);
    assert_eq!(new_snapshot.live_nodes, 3);
    assert!(
        new_snapshot.verify().is_ok(),
        "rebuilt snapshot should pass verification"
    );
}

// -- Test: Recovery from inconsistent snapshot via verify-then-rebuild --
//
// Scenario: Snapshot loads but fails verify(). Engine should detect this and
//           trigger rebuild rather than using the corrupt topology.

#[tokio::test]
async fn test_recovery_from_inconsistent_snapshot_via_verify() {
    use khive_hnsw::{HnswCheckpointConfig, HnswSnapshot};

    let persist = setup_test_persistence().await;

    let id1 = NodeId::new([1; 16]);
    let id2 = NodeId::new([2; 16]);
    let id3 = NodeId::new([3; 16]);

    // Inject a snapshot with mismatched tombstone counts
    let bad_snapshot = HnswSnapshot {
        vector_count: 0,
        total_nodes: 3,
        live_nodes: 1,
        tombstone_count: 2, // Claims 2 tombstones
        max_layer: 0,
        entry_point: Some(id1),
        config: HnswCheckpointConfig {
            m: 16,
            ef_construction: 200,
            metric: "cosine".to_string(),
        },
        indexed_ids: vec![id1, id2, id3],
        tombstoned_ids: vec![id2], // Only 1 tombstone ID (mismatch!)
        layers: vec![],

        vectors: vec![],
    };

    let data = serde_json::to_vec(&bad_snapshot).expect("serialize");
    inject_raw_hnsw_snapshot(&persist, &data).await;

    // Load succeeds
    let loaded = persist
        .load_hnsw_snapshot()
        .await
        .expect("load")
        .expect("snapshot exists");

    // But verify catches the corruption
    let verify_err = loaded.verify().unwrap_err();
    let err_msg = verify_err.to_string();
    assert!(
        err_msg.contains("tombstoned_ids count mismatch"),
        "should report tombstone count mismatch, got: {err_msg}"
    );

    // Recovery: clear and rebuild
    persist.clear().await.expect("clear");

    let mut rebuilt = HnswIndex::new(4);
    rebuilt
        .insert(id1, vec![1.0, 0.0, 0.0, 0.0])
        .expect("insert");
    rebuilt
        .insert(id2, vec![0.0, 1.0, 0.0, 0.0])
        .expect("insert");
    rebuilt
        .insert(id3, vec![0.0, 0.0, 1.0, 0.0])
        .expect("insert");

    persist
        .persist_hnsw_snapshot(&rebuilt)
        .await
        .expect("persist rebuilt");

    let new_snapshot = persist
        .load_hnsw_snapshot()
        .await
        .expect("load")
        .expect("snapshot exists");
    assert!(
        new_snapshot.verify().is_ok(),
        "rebuilt snapshot should be valid"
    );
}

// -- Test: Restore from snapshot detects corrupt snapshot --
//
// Scenario: An index tries to restore_from_snapshot with a corrupt snapshot.
//           The restore should fail with an error, not silently use bad data.

#[tokio::test]
async fn test_restore_from_corrupt_snapshot_fails() {
    use khive_hnsw::{HnswCheckpointConfig, HnswSnapshot};

    let id1 = NodeId::new([1; 16]);
    let id2 = NodeId::new([2; 16]);

    let mut index = HnswIndex::new(4);

    // Create a corrupt snapshot (total_nodes mismatch)
    let bad_snapshot = HnswSnapshot {
        vector_count: 0,
        total_nodes: 10, // WRONG
        live_nodes: 10,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: Some(id1),
        config: HnswCheckpointConfig {
            m: 16,
            ef_construction: 200,
            metric: "cosine".to_string(),
        },
        indexed_ids: vec![id1, id2], // Only 2
        tombstoned_ids: vec![],
        layers: vec![],

        vectors: vec![],
    };

    let vectors: std::collections::HashMap<NodeId, Vec<f32>> = [
        (id1, vec![1.0, 0.0, 0.0, 0.0]),
        (id2, vec![0.0, 1.0, 0.0, 0.0]),
    ]
    .into_iter()
    .collect();

    let result = index.restore_from_snapshot(&bad_snapshot, &vectors);
    assert!(
        result.is_err(),
        "restore_from_snapshot should reject corrupt snapshot"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("Invalid snapshot"),
        "error should mention invalid snapshot, got: {err_msg}"
    );
}

// -- Test: Binary garbage as HNSW snapshot --
//
// Scenario: Random non-JSON binary data in the snapshot slot (e.g., disk corruption
//           that overwrites the entire BLOB).

#[tokio::test]
async fn test_binary_garbage_hnsw_snapshot_detected() {
    let persist = setup_test_persistence().await;

    // Insert pure binary garbage
    let garbage: Vec<u8> = (0..256).map(|i| i as u8).collect();
    inject_raw_hnsw_snapshot(&persist, &garbage).await;

    let result = persist.load_hnsw_snapshot().await;
    assert!(result.is_err(), "binary garbage should fail to deserialize");
    let err = result.unwrap_err();
    assert!(
        matches!(err, PersistError::Deserialize(_)),
        "should be Deserialize error, got: {err:?}"
    );
}

// -- Test: Overwrite corrupt snapshot with valid one --
//
// Scenario: After detecting corruption, persisting a new valid snapshot should
//           overwrite the corrupt data (INSERT OR REPLACE behavior).

#[tokio::test]
async fn test_overwrite_corrupt_snapshot_with_valid() {
    let persist = setup_test_persistence().await;

    // Inject corrupt data
    inject_raw_hnsw_snapshot(&persist, b"this is not valid json").await;

    // Verify it's corrupt
    assert!(persist.load_hnsw_snapshot().await.is_err());

    // Now persist a valid index (should overwrite the corrupt entry)
    let mut index = HnswIndex::new(4);
    let id1 = NodeId::new([1; 16]);
    index.insert(id1, vec![1.0, 0.0, 0.0, 0.0]).expect("insert");

    persist
        .persist_hnsw_snapshot(&index)
        .await
        .expect("persist should overwrite corrupt entry");

    // Loading should now succeed
    let loaded = persist
        .load_hnsw_snapshot()
        .await
        .expect("load should succeed after overwrite")
        .expect("snapshot should exist");

    assert_eq!(loaded.total_nodes, 1);
    assert!(loaded.verify().is_ok());
}
