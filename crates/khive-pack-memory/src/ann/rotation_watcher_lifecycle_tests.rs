#[tokio::test(start_paused = true)]
#[serial(background_tasks)]
async fn rotation_watcher_exits_on_local_shutdown_without_advancing_time() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: Some(dir.path().join("rotation-shutdown.db")),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("writable runtime");
    let ann = new_shared();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let started_at = tokio::time::Instant::now();

    let watcher = start_rotation_watcher_with_shutdown(&rt, &ann, shutdown.clone())
        .expect("file-backed ANN state starts a watcher");
    assert!(rotation_watch_started_for_test(&ann));
    assert!(
        !watcher.is_finished(),
        "newly started watcher must stay active"
    );
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(
        !watcher.is_finished(),
        "uncancelled watcher must stay active"
    );

    shutdown.cancel();
    for _ in 0..100 {
        if watcher.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
    }
    if !watcher.is_finished() {
        watcher.abort();
        let _ = watcher.await;
        panic!("local shutdown must stop the watcher while its ANN state is alive");
    }
    watcher
        .await
        .expect("rotation watcher must finish successfully after local shutdown");
    assert_eq!(Arc::strong_count(&ann), 1);
    assert_eq!(tokio::time::Instant::now(), started_at);
}
