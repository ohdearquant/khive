use super::*;

use khive_pack_kg::KgPack;
use khive_runtime::{Namespace, RuntimeConfig, VerbRegistryBuilder};
use serial_test::serial;

use crate::test_support::HashVecProvider;

/// The second recall uses the installed ANN index, measured by its route counter.
/// See `crates/khive-pack-memory/docs/api/ann-lifecycle.md`.
#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn recall_second_call_uses_warm_ann_route() {
    let tmp = tempfile::Builder::new()
        .prefix("khive-memory-ann-route-")
        .tempdir_in(std::env::temp_dir())
        .expect("temp /tmp db dir");
    let db_path = tmp.path().join("khive-graph.db");

    const MODEL: &str = "ann-route-test-model";
    const DIMS: usize = 32;

    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(db_path),
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("runtime");
    rt.register_embedder(HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: DIMS,
    });

    let ns = Namespace::parse("local").expect("local namespace");
    let token = rt.authorize(ns).expect("authorize local");

    // Create notes with embedding_model: None so the runtime auto-detects
    // the registered custom provider (resolve_embedding_model only handles
    // lattice aliases; custom provider names must go through the auto-detect path).
    for i in 0..32u32 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("ann warm route note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create note");
    }

    let pack = MemoryPack::new(rt.clone());
    let ann = pack.ann_for_test();

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(pack);
    let registry = builder.build().expect("registry");

    // First recall: triggers synchronous ANN build on cache miss.
    // No explicit embedding_model — auto-detects ann-route-test-model.
    registry
        .dispatch(
            "memory.recall",
            serde_json::json!({
                "query": "ann warm route note 7",
                "limit": 10
            }),
        )
        .await
        .expect("first recall");

    ann.reset_warm_route_count();

    // Second recall: index is already loaded — must go through warm ANN.
    registry
        .dispatch(
            "memory.recall",
            serde_json::json!({
                "query": "ann warm route note 7",
                "limit": 10
            }),
        )
        .await
        .expect("second recall");

    assert!(
        ann.warm_route_count() > 0,
        "second recall must route through warm ANN, not exact sqlite-vec fallback"
    );
}
