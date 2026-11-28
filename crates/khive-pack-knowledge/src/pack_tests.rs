use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use lattice_embed::EmbeddingModel;

use super::KnowledgePack;

// ADR-103 Amendment 1 Part 2: knowledge_embedder_warm phase-span events.
//
// `KhiveRuntime::memory()` builds on `RuntimeConfig::no_embeddings()`
// (runtime.rs), so `default_embedder_name()` is empty and the
// configured-embedder branch in `KnowledgePack::warm()` never runs. This
// pins that gating: the unconditional `vamana::warm_known_snapshots` call
// above the branch must never itself emit a phase event, and the
// configured-only branch must not fire when unconfigured.
#[tokio::test]
async fn knowledge_warm_emits_no_phase_events_when_no_embedder_configured() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let event_store = rt.events(&token).expect("event store must be available");

    let pack = KnowledgePack::new(rt.clone());
    pack.warm().await;

    let page = event_store
        .query_events(
            khive_storage::EventFilter::default(),
            khive_storage::PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .expect("query events");

    assert!(
        page.items.is_empty(),
        "no embedder configured -> KnowledgePack::warm() must emit zero phase events \
             (configured-only branch must not fire, and warm_known_snapshots must not \
             independently emit): {:?}",
        page.items
    );
}

#[tokio::test]
async fn read_only_knowledge_cache_miss_does_not_start_ann_writer_lifecycle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = RuntimeConfig {
        db_path: Some(dir.path().join("read-only-knowledge-ann.db")),
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        additional_embedding_models: vec![],
        ..RuntimeConfig::no_embeddings()
    };
    drop(KhiveRuntime::new(config.clone()).expect("migrate snapshot source"));

    #[cfg(unix)]
    {
        let db_path = config.db_path.as_ref().expect("db path");
        khive_storage::test_support::freeze_snapshot_sidecars(db_path);
    }

    let read_only = KhiveRuntime::new_readonly(config).expect("open snapshot read-only");
    let token = read_only
        .authorize(Namespace::local())
        .expect("authorize local");
    let pack = KnowledgePack::new(read_only.clone());
    let key = crate::knowledge::vamana::AnnKey::new(
        token.namespace().as_str(),
        read_only.default_embedder_name(),
    );
    let before = read_only.backend().pool().writer_acquisition_snapshot();

    crate::knowledge::vamana::ensure_ann_background(&read_only, &token, &pack.ann);

    assert!(
        !crate::knowledge::vamana::is_warming_not_loaded(&pack.ann, &key),
        "a read-only search cache miss must not claim or spawn an ANN warm slot"
    );
    assert_eq!(
        read_only.backend().pool().writer_acquisition_snapshot(),
        before,
        "read-only ANN admission must not acquire a writer"
    );
}

/// Pack-level lifecycle coverage for `start_rotation_watcher`'s
/// production wiring (issue #2340): the rotation tests elsewhere in this
/// crate call the private `refresh_rotated_segments_once` test helper
/// directly, so nothing previously asserted that a writable `warm()`
/// actually starts the tracked watcher, that a repeated `warm()` is
/// idempotent, or that the watcher exits once its `AnnState` is dropped.
/// Time is paused so the watcher's 5-second tick can be crossed
/// deterministically instead of by a real sleep.
#[tokio::test(start_paused = true)]
#[serial_test::serial(background_tasks)]
async fn writable_warm_starts_one_tracked_watcher_idempotently_and_it_exits_on_drop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = RuntimeConfig {
        db_path: Some(dir.path().join("rotation-watcher-lifecycle.db")),
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::no_embeddings()
    };
    let rt = KhiveRuntime::new(config).expect("writable runtime");
    let pack = KnowledgePack::new(rt);

    let before = khive_runtime::background_task_count();
    pack.warm().await;
    assert!(
        crate::knowledge::vamana::rotation_watch_started_for_test(&pack.ann),
        "a writable warm must start the rotation watcher"
    );
    assert_eq!(
        khive_runtime::background_task_count(),
        before + 1,
        "warm must track exactly one rotation watcher task"
    );

    // A repeated warm must not start a second tracked watcher.
    pack.warm().await;
    assert_eq!(
        khive_runtime::background_task_count(),
        before + 1,
        "a repeated warm must be idempotent and not start a second watcher"
    );

    // The watcher retains only a Weak<AnnState>; dropping every strong
    // reference lets its next tick observe the failed upgrade and exit.
    let ann_weak = std::sync::Arc::downgrade(&pack.ann);
    drop(pack);
    assert!(
        ann_weak.upgrade().is_none(),
        "dropping the pack must drop its last strong ANN reference"
    );

    // The watcher builds its interval on its first poll, which can land
    // after this advance; under paused time `sleep` auto-advances the
    // clock once every task is idle, so the tick is reached by virtual
    // time rather than by a fixed yield budget.
    tokio::time::advance(std::time::Duration::from_secs(6)).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    while khive_runtime::background_task_count() != before {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the watcher must exit once its ANN state is dropped"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        khive_runtime::background_task_count(),
        before,
        "the watcher must exit once its ANN state is dropped"
    );
}
