//! The ANN consumer registry is defined in `khive-retrieval` and re-exported by
//! `khive-runtime`: both crate paths name one set of items, not two copies.

use khive_retrieval::ann::registry as retrieval_registry;
use khive_runtime::{ann_registry as runtime_registry, StorageBackend};

#[tokio::test]
async fn runtime_ann_registry_path_is_the_retrieval_registry() {
    assert_eq!(
        runtime_registry::PENDING_WATERMARK,
        retrieval_registry::PENDING_WATERMARK
    );

    let backend = StorageBackend::memory().expect("memory backend");
    backend.prepare_core_schema().expect("core schema");
    let sql = backend.sql();
    let model = "ann-registry-reexport";

    runtime_registry::register_pending(sql.as_ref(), "reexport", "local", model)
        .await
        .expect("register through the runtime path");

    // A value named through the runtime path is accepted by the retrieval function.
    let activated = retrieval_registry::raise_watermark(
        sql.as_ref(),
        "reexport",
        "local",
        model,
        3,
        runtime_registry::WatermarkAuthority::PendingOrActive,
    )
    .await
    .expect("raise through the retrieval path");
    assert!(
        activated,
        "a pending registration must accept an authority named through the runtime path"
    );

    // A value named through the retrieval path is accepted by the runtime function.
    let advanced = runtime_registry::raise_watermark(
        sql.as_ref(),
        "reexport",
        "local",
        model,
        4,
        retrieval_registry::WatermarkAuthority::Active,
    )
    .await
    .expect("raise through the runtime path");
    assert!(
        advanced,
        "an active registration must accept an authority named through the retrieval path"
    );
}
