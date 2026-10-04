//! A recall whose serve-ledger write is dropped at the pending limit still
//! returns its results. This file holds a single test so the process-wide
//! ledger bound is configured and saturated before anything else uses it.

use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistryBuilder};
use serde_json::json;

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn recall_results_are_unchanged_when_the_ledger_write_is_dropped() {
    std::env::set_var("KHIVE_RECALL_LEDGER_MAX_PENDING", "1");

    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(rt));
    let registry = builder.build().expect("registry builds");

    // A held ledger task occupies the only pending slot, as a stalled writer would.
    khive_runtime::track_recall_ledger_task(std::future::pending::<()>());
    let before = khive_runtime::recall_ledger_snapshot();
    assert_eq!(before.max_pending, 1);
    assert_eq!(before.pending, 1, "the held task occupies the only slot");
    assert_eq!(before.skipped, 0);

    let remembered = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "The attention mechanism in transformers uses Q K V matrices",
                "memory_type": "semantic",
                "salience": 0.8,
                "decay": 0.01
            }),
        )
        .await
        .expect("memory.remember succeeds");
    let note_id = remembered["id"].as_str().expect("has note_id");

    let recalled = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "attention mechanism transformers" }),
        )
        .await
        .expect("memory.recall succeeds with its ledger write dropped");
    let hits = recalled.as_array().expect("array of hits");
    assert!(!hits.is_empty(), "recall returned at least one result");
    assert_eq!(
        hits[0]["id"].as_str().unwrap(),
        note_id,
        "recalled the memory we just created"
    );

    let after = khive_runtime::recall_ledger_snapshot();
    assert!(
        after.skipped > before.skipped,
        "the recall's ledger write was dropped at the limit"
    );
    assert_eq!(after.pending, 1, "the dropped write took no slot");
    assert_eq!(after.timed_out, 0);
}
