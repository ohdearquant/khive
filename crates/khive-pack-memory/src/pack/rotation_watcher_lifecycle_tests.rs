use super::*;

use khive_runtime::RuntimeConfig;
use serial_test::serial;

#[tokio::test(start_paused = true)]
#[serial(background_tasks)]
async fn writable_warm_starts_one_tracked_watcher_idempotently_and_it_exits_on_drop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = RuntimeConfig {
        db_path: Some(dir.path().join("rotation-watcher-lifecycle.db")),
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    };
    let rt = KhiveRuntime::new(config).expect("writable runtime");
    let pack = MemoryPack::new(rt);
    let ann = pack.ann_for_test();

    pack.warm().await;
    assert!(
        crate::ann::rotation_watch_started_for_test(&ann),
        "a writable warm must start the rotation watcher"
    );
    let watcher = crate::ann::take_rotation_watch_handle_for_test(&ann)
        .expect("a writable warm must retain its rotation watcher task");
    assert!(
        !watcher.is_finished(),
        "the rotation watcher must be running after warm"
    );

    // A repeated warm must not start a second watcher: the one-shot guard
    // is already claimed, so no new task handle is retained.
    pack.warm().await;
    assert!(
        crate::ann::take_rotation_watch_handle_for_test(&ann).is_none(),
        "a repeated warm must be idempotent and not start a second watcher"
    );

    // The watcher retains only a Weak<AnnState>; dropping every strong
    // reference lets its next tick observe the failed upgrade and exit.
    let ann_weak = std::sync::Arc::downgrade(&ann);
    drop(ann);
    drop(pack);
    assert!(
        ann_weak.upgrade().is_none(),
        "dropping the pack and the test's own clone must drop the last strong ANN reference"
    );

    tokio::time::advance(std::time::Duration::from_secs(6)).await;
    let limit = std::time::Duration::from_secs(30);
    let exited = tokio::time::timeout(limit, watcher).await;
    assert!(
        matches!(exited, Ok(Ok(()))),
        "the watcher must exit once its ANN state is dropped: {exited:?}"
    );
}
