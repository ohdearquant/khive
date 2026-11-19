use super::*;

use crate::test_support::HashVecProvider;
use khive_runtime::RuntimeConfig;
use khive_storage::types::{SqlStatement, SqlValue};

#[tokio::test]
async fn read_only_warm_keeps_ann_lifecycle_out_of_the_writer_plane() {
    const MODEL: &str = "read-only-warm-model";
    const DIMS: usize = 8;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("read-only-memory-warm.db");
    let cfg = RuntimeConfig {
        db_path: Some(path),
        ..RuntimeConfig::no_embeddings()
    };

    let writable = KhiveRuntime::new(cfg.clone()).expect("writable snapshot source");
    writable.register_embedder(HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: DIMS,
    });
    let token = writable
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");
    writable
        .vectors_for_model(&token, MODEL)
        .expect("materialize optional vector table");
    let mut writer = writable.sql().writer().await.expect("seed writer");
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO ann_consumer_watermark \
                      (consumer, namespace, embedding_model, watermark) \
                      VALUES (?1, ?2, ?3, 0)"
                .into(),
            params: vec![
                SqlValue::Text("memory-notes:note.content".into()),
                SqlValue::Text("*".into()),
                SqlValue::Text(MODEL.into()),
            ],
            label: Some("seed_read_only_warm_consumer".into()),
        })
        .await
        .expect("seed active consumer");
    drop(writer);

    // `vectors_for_model` and `SqlAccess::writer` lazily spawn the
    // file-backed pool's shared writer task. Own its one-shot join before
    // dropping the runtime, then wait for the queue and close-time WAL
    // work to settle before reopening the same file read-only. Otherwise
    // this regression can observe fixture teardown instead of the
    // read-only warm path it is meant to discriminate.
    let writer_join = writable.backend().pool().take_writer_task_join();
    drop(token);
    drop(writable);
    if let Some(join) = writer_join {
        tokio::time::timeout(std::time::Duration::from_secs(5), join)
            .await
            .expect("writable fixture writer task must drain before read-only reopen")
            .expect("writable fixture writer task must not panic");
    }

    let read_only = KhiveRuntime::new_readonly(cfg).expect("read-only snapshot runtime");
    read_only.register_embedder(HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: DIMS,
    });
    let before = read_only.backend().pool().writer_acquisition_snapshot();

    let pack = MemoryPack::new(read_only.clone());
    pack.warm().await;
    let read_token = read_only
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize read-only local");
    assert!(
        !crate::ann::ensure_ann_background(&read_only, &read_token, &pack.ann, MODEL).await,
        "a read-only recall cache miss must not enqueue a writer-bearing ANN task"
    );

    assert_eq!(
        read_only.backend().pool().writer_acquisition_snapshot(),
        before,
        "daemon warm must not acquire a writer for ANN registration, checkpoint, or \
             optional-table inspection on a read-only runtime"
    );
}
