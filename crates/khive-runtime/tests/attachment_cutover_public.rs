//! A host that embeds `khive-runtime` alone can bring a pre-V21 database
//! forward: `KhiveRuntime::new` refuses it with a typed error that names the
//! public coordinator, the coordinator completes the cutover, and the runtime
//! then constructs.

use std::sync::Arc;

use khive_db::migrations::{AttachmentCutoverStatus, ATTACHMENT_CUTOVER_VERSION};
use khive_runtime::attachment_cutover::coordinate_attachment_cutover;
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError, StorageBackend};

#[path = "support/v20_fixture.rs"]
mod v20_fixture;
use v20_fixture::create_v20_database_fixture;

fn config(db_path: &std::path::Path) -> RuntimeConfig {
    RuntimeConfig {
        db_path: Some(db_path.to_path_buf()),
        events_split: None,
        ..RuntimeConfig::no_embeddings()
    }
}

#[tokio::test]
async fn runtime_refuses_v20_then_public_coordinator_unblocks_it() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("v20.db");
    create_v20_database_fixture(&db_path, "visual_asset", None);

    // 1. The runtime refuses a pre-V21 database with the typed variant.
    let refusal = match KhiveRuntime::new(config(&db_path)) {
        Ok(_) => panic!("KhiveRuntime::new must refuse a database below the V21 cutover"),
        Err(error) => error,
    };
    match &refusal {
        RuntimeError::AttachmentCutoverRequired {
            schema_version,
            required_version,
        } => {
            assert_eq!(*required_version, ATTACHMENT_CUTOVER_VERSION);
            assert_eq!(*schema_version, ATTACHMENT_CUTOVER_VERSION - 1);
        }
        other => panic!("expected AttachmentCutoverRequired, got {other:?}"),
    }
    assert!(
        refusal
            .to_string()
            .contains("khive_runtime::coordinate_attachment_cutover"),
        "the refusal must name the public coordinator: {refusal}"
    );

    // 2. The public coordinator completes the cutover.
    let backend = Arc::new(StorageBackend::sqlite(&db_path).unwrap());
    coordinate_attachment_cutover(Arc::clone(&backend), None, None)
        .await
        .expect("non-model legacy content needs no BlobHydrator");
    assert_eq!(
        backend.attachment_cutover_status().unwrap(),
        AttachmentCutoverStatus::Complete
    );
    drop(backend);

    // 3. The same database now constructs a runtime.
    let runtime = match KhiveRuntime::new(config(&db_path)) {
        Ok(runtime) => runtime,
        Err(error) => panic!("runtime must construct after the cutover: {error}"),
    };
    assert_eq!(
        runtime.backend().attachment_cutover_status().unwrap(),
        AttachmentCutoverStatus::Complete
    );
}
