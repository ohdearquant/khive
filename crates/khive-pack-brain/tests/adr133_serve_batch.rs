//! ADR-133 D7 proof: `brain.record_serve` already routes every target id from
//! one dispatch through exactly one `record_serves` call, so one caller-visible
//! writer acquisition covers the whole batch regardless of target count, and
//! an empty batch never touches the writer at all.

use std::sync::Arc;

use khive_pack_brain::BrainPack;
use khive_runtime::{
    BackendId, KhiveRuntime, Namespace, PackRuntime, RuntimeConfig, VerbRegistryBuilder,
};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::SqlAccess;
use serde_json::json;

fn file_backed_runtime(db_path: std::path::PathBuf) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        git_write: Default::default(),
        db_path: Some(db_path),
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(khive_runtime::AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
    })
    .expect("file-backed runtime")
}

async fn serve_ledger_row_count(sql: &dyn SqlAccess) -> i64 {
    let mut reader = sql.reader().await.expect("reader");
    match reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM brain_serve_ledger".into(),
            params: vec![],
            label: Some("adr133_serve_ledger_row_count".into()),
        })
        .await
        .expect("count query")
    {
        Some(SqlValue::Integer(n)) => n,
        other => panic!("expected integer count, got {other:?}"),
    }
}

/// P2 D7 selector: `brain_record_serve_uses_one_writer_acquisition`
/// (`final_verification_plan_r2.md` §6). Drives the public `brain.record_serve`
/// verb with N unique targets plus one duplicate target id in the same call,
/// and proves: exactly one writer acquisition for the whole batch, correct
/// `written`/`skipped` counts, matching persisted row count, and zero
/// acquisitions for an empty batch. Production `handlers.rs`/`serve_ledger.rs`
/// are read-only for this proof.
#[tokio::test]
async fn brain_record_serve_uses_one_writer_acquisition() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("adr133-d7-serve-batch.db");
    let rt = file_backed_runtime(db_path);
    let brain = Arc::new(BrainPack::new(rt.clone()));
    let registry = VerbRegistryBuilder::new().build().expect("registry");
    let token = rt.authorize(Namespace::local()).expect("token");
    let sql = rt.sql();

    let before = rt.db_diagnostics().await.expect("diagnostics before");

    let out = brain
        .dispatch(
            "brain.record_serve",
            json!({
                "consumer_kind": "recall",
                "query_raw": "adr-133 d7 proof query",
                "target_ids": ["target-a", "target-b", "target-c", "target-a"],
            }),
            &registry,
            &token,
        )
        .await
        .expect("record_serve dispatch");

    let after = rt.db_diagnostics().await.expect("diagnostics after");

    let writer_delta = after
        .writer_contention
        .writer_acquisitions
        .checked_sub(before.writer_contention.writer_acquisitions)
        .expect("monotonic writer counter");
    assert_eq!(
        writer_delta, 1,
        "one dispatch with a mixed unique/duplicate batch must cost exactly one \
         writer acquisition; before={before:?} after={after:?}"
    );

    assert_eq!(out.get("written").and_then(|v| v.as_u64()), Some(3));
    assert_eq!(out.get("skipped").and_then(|v| v.as_u64()), Some(1));

    let persisted = serve_ledger_row_count(sql.as_ref()).await;
    assert_eq!(
        persisted, 3,
        "persisted row count must match the batch's written count"
    );

    let before_empty = rt.db_diagnostics().await.expect("diagnostics before empty");
    let out_empty = brain
        .dispatch(
            "brain.record_serve",
            json!({
                "consumer_kind": "recall",
                "query_raw": "adr-133 d7 empty-input proof",
                "target_ids": [],
            }),
            &registry,
            &token,
        )
        .await
        .expect("record_serve dispatch with empty input");
    let after_empty = rt.db_diagnostics().await.expect("diagnostics after empty");

    assert_eq!(out_empty.get("written").and_then(|v| v.as_u64()), Some(0));
    assert_eq!(out_empty.get("skipped").and_then(|v| v.as_u64()), Some(0));
    assert_eq!(
        after_empty.writer_contention.writer_acquisitions,
        before_empty.writer_contention.writer_acquisitions,
        "an empty target_ids batch must not acquire the writer at all"
    );

    let persisted_after_empty = serve_ledger_row_count(sql.as_ref()).await;
    assert_eq!(
        persisted_after_empty, 3,
        "the empty-input dispatch must not change the persisted row count"
    );
}
