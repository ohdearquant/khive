use super::*;
use crate::{Namespace, RuntimeConfig};
use chrono::Utc;
use serde_json::json;

fn fixture() -> (tempfile::TempDir, KhiveRuntime, NamespaceToken) {
    let dir = tempfile::tempdir().expect("database directory");
    let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
        db_path: Some(dir.path().join("batch-edges.db")),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("file-backed runtime");
    let token = NamespaceToken::for_namespace(Namespace::local());
    (dir, runtime, token)
}

fn edge(namespace: &str, serial: u128) -> Edge {
    let now = Utc::now();
    Edge {
        id: LinkId::from(Uuid::from_u128(serial)),
        namespace: namespace.into(),
        source_id: Uuid::from_u128(serial + 10_000),
        target_id: Uuid::from_u128(serial + 20_000),
        relation: EdgeRelation::Supports,
        weight: 1.0,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: Some(json!({"serial": serial.to_string()})),
        target_backend: None,
    }
}

async fn seed(runtime: &KhiveRuntime, namespace: &str, edges: Vec<Edge>) {
    let token = NamespaceToken::for_namespace(Namespace::parse(namespace).expect("namespace"));
    runtime
        .graph(&token)
        .expect("graph")
        .upsert_edges(edges)
        .await
        .expect("seed edges");
}

fn rewrite(runtime: &KhiveRuntime, id: Uuid, assignment: &str) {
    let writer = runtime.backend().pool().try_writer().expect("writer");
    writer
        .conn()
        .execute(
            &format!("UPDATE graph_edges SET {assignment} WHERE id=?1"),
            [id.to_string()],
        )
        .expect("real stored-row mutation");
}

fn assert_row_driver(error: &RuntimeError, expected_source: &str) {
    let RuntimeError::Storage(khive_storage::StorageError::Driver {
        operation, source, ..
    }) = error
    else {
        panic!("expected attributed row Driver, got {error:?}");
    };
    assert_eq!(
        operation.as_ref(),
        "get_edge",
        "primitive diagnostic label must survive"
    );
    assert!(source.to_string().contains(expected_source), "{error:?}");
}

#[tokio::test]
async fn batch_edges_align_foreign_missing_deleted_and_duplicate_ids() {
    let (_dir, runtime, token) = fixture();
    let local = edge("local", 3);
    let foreign = edge("foreign", 1);
    let mut deleted = edge("foreign", 2);
    deleted.deleted_at = Some(Utc::now());
    seed(&runtime, "local", vec![local.clone()]).await;
    seed(&runtime, "foreign", vec![foreign.clone(), deleted.clone()]).await;
    let ids = [
        Uuid::from(local.id),
        Uuid::from(foreign.id),
        Uuid::from_u128(99),
        Uuid::from(deleted.id),
        Uuid::from(local.id),
    ];
    let rows = runtime.get_edges_by_id(&token, &ids).await.expect("batch");
    assert_eq!(rows.len(), ids.len());
    assert_eq!(rows[0].as_ref().unwrap().source_id, local.source_id);
    assert_eq!(rows[1].as_ref().unwrap().source_id, foreign.source_id);
    assert!(rows[2].is_none() && rows[3].is_none());
    assert_eq!(rows[4].as_ref().unwrap().id, local.id);
    let window = runtime.prepare_edge_read_window(&ids).await.unwrap();
    let mut selected_namespaces = Vec::new();
    KhiveRuntime::hydrate_edge_read_window(&ids, window, |record_token| {
        selected_namespaces.push(record_token.namespace().as_str().to_owned());
        runtime.graph(record_token)
    })
    .await
    .unwrap();
    assert_eq!(selected_namespaces, ["local", "foreign"]);
    assert!(runtime
        .get_edges_by_id(&token, &[])
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn batch_edge_outcomes_keep_actual_corrupt_rows_at_requested_positions() {
    let (_dir, runtime, token) = fixture();
    let good = edge("local", 10);
    let corrupt = edge("local", 11);
    seed(&runtime, "local", vec![good.clone(), corrupt.clone()]).await;
    rewrite(&runtime, corrupt.id.into(), "metadata='{' ");
    let ids = [
        good.id,
        corrupt.id,
        LinkId::from(Uuid::from_u128(500)),
        corrupt.id,
        good.id,
    ];
    let rows = runtime
        .graph(&token)
        .unwrap()
        .get_edge_read_outcomes(&ids)
        .await
        .unwrap();
    assert_eq!(rows.len(), ids.len());
    assert_eq!(rows[0].as_ref().unwrap().as_ref().unwrap().id, good.id);
    assert!(rows[1].is_err() && rows[3].is_err());
    assert!(rows[2].as_ref().unwrap().is_none());
    assert_eq!(rows[4].as_ref().unwrap().as_ref().unwrap().id, good.id);
    let error = runtime
        .get_edges_by_id(&token, &[corrupt.id.into()])
        .await
        .unwrap_err();
    assert_row_driver(&error, "EOF");
    assert!(
        error.to_string().contains("EOF") && error.to_string().contains("get_edge"),
        "{error:?}"
    );
}

#[tokio::test]
async fn batch_edge_errors_follow_input_order_across_namespace_groups() {
    let (_dir, runtime, token) = fixture();
    let good_a = edge("group-a", 30);
    let corrupt_a = edge("group-a", 31);
    let corrupt_b = edge("group-b", 32);
    seed(&runtime, "group-a", vec![good_a.clone(), corrupt_a.clone()]).await;
    seed(&runtime, "group-b", vec![corrupt_b.clone()]).await;
    rewrite(&runtime, corrupt_a.id.into(), "metadata='{' ");
    rewrite(&runtime, corrupt_b.id.into(), "weight='bad-weight'");
    let ids = [good_a.id.into(), corrupt_b.id.into(), corrupt_a.id.into()];
    for reverse_groups in [false, true] {
        let mut window = runtime.prepare_edge_read_window(&ids).await.unwrap();
        if reverse_groups {
            window.groups.reverse();
        }
        let error = KhiveRuntime::hydrate_edge_read_window(&ids, window, |record_token| {
            runtime.graph(record_token)
        })
        .await
        .unwrap_err();
        assert_row_driver(&error, "weight");
        assert!(
            error.to_string().contains("weight"),
            "earlier input must win: {error:?}"
        );
    }
    let reversed = [corrupt_a.id.into(), corrupt_b.id.into(), good_a.id.into()];
    let error = runtime
        .get_edges_by_id(&token, &reversed)
        .await
        .unwrap_err();
    assert_row_driver(&error, "EOF");
    assert!(
        error.to_string().contains("EOF") && error.to_string().contains("get_edge"),
        "{error:?}"
    );
}

#[tokio::test]
async fn batch_edge_namespace_validation_precedes_same_row_decode() {
    let (_dir, runtime, token) = fixture();
    let bad_ns = edge("local", 40);
    let bad_json = edge("local", 41);
    seed(&runtime, "local", vec![bad_ns.clone(), bad_json.clone()]).await;
    rewrite(
        &runtime,
        bad_ns.id.into(),
        "namespace='invalid!', metadata='{' ",
    );
    rewrite(&runtime, bad_json.id.into(), "metadata='{' ");
    let error = runtime
        .get_edges_by_id(&token, &[bad_ns.id.into(), bad_json.id.into()])
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("edge namespace invalid"),
        "{error:?}"
    );
    let error = runtime
        .get_edges_by_id(&token, &[bad_json.id.into(), bad_ns.id.into()])
        .await
        .unwrap_err();
    assert_row_driver(&error, "EOF");
    assert!(
        error.to_string().contains("EOF") && error.to_string().contains("get_edge"),
        "{error:?}"
    );
}

#[tokio::test]
async fn batch_edge_decode_error_beats_later_actual_accessor_refusal() {
    let (_dir, runtime, _token) = fixture();
    let corrupt = edge("first-decode", 50);
    let refused = edge("second-refused", 51);
    seed(&runtime, "first-decode", vec![corrupt.clone()]).await;
    seed(&runtime, "second-refused", vec![refused.clone()]).await;
    rewrite(&runtime, corrupt.id.into(), "metadata='{' ");
    let ids = [corrupt.id.into(), refused.id.into()];
    let window = runtime.prepare_edge_read_window(&ids).await.unwrap();
    let mut accessor_refused = false;
    let error = KhiveRuntime::hydrate_edge_read_window(&ids, window, |record_token| {
        if record_token.namespace().as_str() == "second-refused" {
            let writer = runtime.backend().pool().try_writer().unwrap();
            writer
                .conn()
                .execute_batch(
                    "DROP INDEX IF EXISTS idx_graph_edges_ns_src_rel; PRAGMA query_only=ON",
                )
                .unwrap();
            drop(writer);
            let result = runtime.graph(record_token);
            accessor_refused = result.is_err();
            return result;
        }
        runtime.graph(record_token)
    })
    .await
    .unwrap_err();
    {
        let writer = runtime.backend().pool().try_writer().unwrap();
        writer
            .conn()
            .execute_batch("PRAGMA query_only=OFF")
            .unwrap();
    }
    assert!(
        accessor_refused,
        "actual SQLite DDL refusal must be observed"
    );
    assert_row_driver(&error, "EOF");
    assert!(
        error.to_string().contains("EOF"),
        "earlier row failure must beat accessor refusal: {error:?}"
    );
}

#[tokio::test]
async fn batch_edge_accessor_failure_keeps_group_first_index_when_group_order_reverses() {
    let (_dir, runtime, _token) = fixture();
    let refused = edge("first-refused", 55);
    let corrupt = edge("second-decode", 56);
    seed(&runtime, "first-refused", vec![refused.clone()]).await;
    seed(&runtime, "second-decode", vec![corrupt.clone()]).await;
    rewrite(&runtime, corrupt.id.into(), "metadata='{' ");
    let ids = [refused.id.into(), corrupt.id.into()];
    for reverse_groups in [false, true] {
        let mut window = runtime.prepare_edge_read_window(&ids).await.unwrap();
        if reverse_groups {
            window.groups.reverse();
        }
        let mut actual_refusal = false;
        let error =
            KhiveRuntime::hydrate_edge_read_window(&ids, window, |record_token| {
                if record_token.namespace().as_str() == "first-refused" {
                    {
                        let writer = runtime.backend().pool().try_writer().unwrap();
                        writer.conn().execute_batch(
                        "DROP INDEX IF EXISTS idx_graph_edges_ns_src_rel; PRAGMA query_only=ON"
                    ).unwrap();
                    }
                    let result = runtime.graph(record_token);
                    actual_refusal = result.is_err();
                    {
                        let writer = runtime.backend().pool().try_writer().unwrap();
                        writer
                            .conn()
                            .execute_batch("PRAGMA query_only=OFF")
                            .unwrap();
                    }
                    return result;
                }
                runtime.graph(record_token)
            })
            .await
            .unwrap_err();
        assert!(actual_refusal);
        assert!(
            error.to_string().contains("readonly"),
            "group's first input must win regardless of group processing order: {error:?}"
        );
    }
}

#[tokio::test]
async fn batch_edge_metadata_and_hydration_keep_delete_restore_observations() {
    let (_dir, runtime, token) = fixture();
    let disappearing = edge("local", 60);
    let mut restoring = edge("local", 61);
    restoring.deleted_at = Some(Utc::now());
    seed(
        &runtime,
        "local",
        vec![disappearing.clone(), restoring.clone()],
    )
    .await;
    let ids = [disappearing.id.into(), restoring.id.into()];
    let window = runtime.prepare_edge_read_window(&ids).await.unwrap();
    rewrite(&runtime, disappearing.id.into(), "deleted_at=1");
    rewrite(&runtime, restoring.id.into(), "deleted_at=NULL");
    let rows = KhiveRuntime::hydrate_edge_read_window(&ids, window, |record_token| {
        runtime.graph(record_token)
    })
    .await
    .unwrap();
    assert!(
        rows[0].is_none(),
        "deleted after metadata remains a missing endpoint"
    );
    assert!(
        rows[1].is_none(),
        "metadata miss is not retried after restoration"
    );
    assert!(runtime
        .get_edges_by_id(&token, &[restoring.id.into()])
        .await
        .unwrap()[0]
        .is_some());
}

#[tokio::test]
async fn batch_edges_1000_actual_reader_checkouts_are_window_bounded() {
    let (_dir, runtime, token) = fixture();
    let edges: Vec<_> = (100..1100).map(|serial| edge("local", serial)).collect();
    let ids: Vec<Uuid> = edges.iter().rev().map(|edge| edge.id.into()).collect();
    seed(&runtime, "local", edges).await;
    let before = runtime
        .backend()
        .pool()
        .reader_acquisition_snapshot()
        .pooled_checkouts;
    let rows = runtime.get_edges_by_id(&token, &ids).await.unwrap();
    let after = runtime
        .backend()
        .pool()
        .reader_acquisition_snapshot()
        .pooled_checkouts;
    assert_eq!(rows.len(), 1000);
    assert_eq!(rows[0].as_ref().unwrap().id, LinkId::from(ids[0]));
    assert_eq!(
        after - before,
        4,
        "two metadata windows and two actual edge reads"
    );
}

#[tokio::test]
async fn batch_edge_metadata_reader_refusal_is_fatal_without_retry() {
    let (_dir, runtime, token) = fixture();
    let live = edge("local", 1200);
    seed(&runtime, "local", vec![live.clone()]).await;
    let pool = runtime.backend().pool_arc();
    let capacity = pool.reader_acquisition_snapshot().reader_admission_capacity;
    let held: Vec<_> = (0..capacity).map(|_| pool.reader().unwrap()).collect();
    let before = pool.reader_acquisition_snapshot().checkout_timeouts;
    let result = runtime.get_edges_by_id(&token, &[live.id.into()]).await;
    assert!(
        matches!(
            result,
            Err(RuntimeError::Storage(
                khive_storage::StorageError::AdmissionTimeout { .. }
            ))
        ),
        "{result:?}"
    );
    assert_eq!(
        pool.reader_acquisition_snapshot().checkout_timeouts - before,
        1
    );
    drop(held);
}
