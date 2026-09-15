//! A single-test binary keeps the process generation sequence isolated.

use std::sync::Arc;

use khive_db::StorageBackend;
use khive_runtime::{BackendId, KhiveRuntime, RuntimeConfig};

#[tokio::test(flavor = "current_thread")]
async fn diagnostics_main_pool_generation_tracks_runtime_reconstruction() {
    let auxiliary = Arc::new(StorageBackend::memory().unwrap());
    let secondary = KhiveRuntime::from_backend(
        auxiliary,
        RuntimeConfig {
            backend_id: BackendId::parse("secondary").unwrap(),
            ..RuntimeConfig::no_embeddings()
        },
    );
    let main = Arc::new(StorageBackend::memory().unwrap());
    let runtime = KhiveRuntime::from_backend(Arc::clone(&main), RuntimeConfig::no_embeddings());
    let first = runtime.db_diagnostics().await.unwrap();
    assert_eq!(first.process.pool_generation, 1);

    drop(main.pool().try_writer().unwrap());
    drop(main.pool().reader().unwrap());
    let repeated = runtime.clone().db_diagnostics().await.unwrap();
    assert_eq!(repeated.process, first.process);
    assert!(repeated.writer_contention.writer_acquisitions > 0);
    assert!(repeated.reader_contention.reader_acquisitions > 0);

    let another_handle =
        KhiveRuntime::from_backend(Arc::clone(&main), RuntimeConfig::no_embeddings());
    assert_eq!(
        another_handle.db_diagnostics().await.unwrap().process,
        first.process
    );
    let routed = secondary.with_core_backend(main);
    assert_eq!(
        routed.db_diagnostics().await.unwrap().process,
        first.process
    );

    // Construction must count even when this generation is never observed.
    drop(KhiveRuntime::from_backend(
        Arc::new(StorageBackend::memory().unwrap()),
        RuntimeConfig::no_embeddings(),
    ));
    let replacement = KhiveRuntime::from_backend(
        Arc::new(StorageBackend::memory().unwrap()),
        RuntimeConfig::no_embeddings(),
    );
    let after = replacement.db_diagnostics().await.unwrap();
    assert_eq!(after.process.pool_generation, 3);
    assert_eq!(after.process.pid, first.process.pid);
    assert_eq!(after.process.started_at, first.process.started_at);
    assert_eq!(after.writer_contention.writer_acquisitions, 0);
    assert_eq!(after.reader_contention.reader_acquisitions, 0);
    assert_eq!(after.checkpoint_counters, first.checkpoint_counters);
    assert_eq!(
        runtime.db_diagnostics().await.unwrap().process,
        first.process
    );
}
