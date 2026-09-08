use khive_runtime::{KhiveRuntime, Namespace};
use khive_storage::types::SqlStatement;

use super::acquire_store;
use crate::test_support::HashVecProvider;

#[tokio::test(flavor = "current_thread")]
async fn store_acquisition_yields_to_the_snapshot_owner_in_the_same_task() {
    const MODEL: &str = "store-access-snapshot-model";

    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    runtime.register_embedder(HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: 8,
    });
    let token = runtime
        .authorize(Namespace::local())
        .expect("authorize local");
    runtime.notes(&token).expect("initialize notes store");
    runtime.events(&token).expect("initialize event store");
    runtime
        .vectors_for_model(&token, MODEL)
        .expect("initialize vector store");

    for store in ["notes", "events", "vectors"] {
        let before = runtime.backend().pool().writer_acquisition_snapshot();
        let mut reader = runtime.sql().reader().await.expect("snapshot reader");
        reader
            .query_all(SqlStatement {
                sql: "BEGIN DEFERRED".into(),
                params: vec![],
                label: Some("store_access_snapshot_begin".into()),
            })
            .await
            .expect("admit snapshot before acquiring store");

        let acquiring_runtime = runtime.clone();
        let acquiring_token = token.clone();
        // Poll acquisition first so a blocking checkout prevents its own
        // snapshot-release sibling from running, even on a larger executor.
        let (acquired, committed) = tokio::join!(
            biased;
            acquire_store("memory.recall.store", move || match store {
                "notes" => acquiring_runtime.notes(&acquiring_token).map(|_| ()),
                "events" => acquiring_runtime.events(&acquiring_token).map(|_| ()),
                "vectors" => acquiring_runtime
                    .vectors_for_model(&acquiring_token, MODEL)
                    .map(|_| ()),
                _ => unreachable!("unknown store constructor"),
            }),
            reader.query_all(SqlStatement {
                sql: "COMMIT".into(),
                params: vec![],
                label: Some("store_access_snapshot_commit".into()),
            }),
        );

        committed.unwrap_or_else(|error| panic!("{store}: release snapshot: {error}"));
        acquired
            .and_then(std::convert::identity)
            .unwrap_or_else(|error| {
                panic!("{store}: acquisition must yield to its snapshot-release sibling: {error}")
            });
        assert_eq!(
            runtime.backend().pool().writer_acquisition_snapshot().timeouts,
            before.timeouts,
            "{store}: acquisition must not exhaust writer checkout while its snapshot owner can run"
        );
    }
}
