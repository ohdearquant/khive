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

#[derive(Clone, Copy)]
enum AcquisitionStop {
    Cancel,
    Drop,
}

fn stopped_acquisition_releases_the_blocking_worker(stop: AcquisitionStop, store: &'static str) {
    use std::time::Duration;

    use khive_runtime::RuntimeError;
    use khive_storage::{scope_request_read_cancellation, StorageError};

    // A marker queued behind acquisition can run only after the real worker
    // returns; completing or dropping the async waiter alone is insufficient.
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("single-worker test runtime");
    executor.block_on(async {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        runtime.register_embedder(HashVecProvider {
            model_name: "cancelled-store-model".to_owned(),
            dims: 8,
        });
        let token = runtime
            .authorize(Namespace::local())
            .expect("authorize local");
        // No accessor for this store kind has run on this backend: cold acquisition.
        let pool = runtime.backend().pool();
        let checkout_timeout = pool.config().checkout_timeout;
        let release_bound = Duration::from_secs(1).min(checkout_timeout / 2);
        assert!(
            release_bound >= Duration::from_millis(100),
            "fixture requires a checkout timeout long enough to distinguish cancellation"
        );
        let before = pool.writer_acquisition_snapshot();
        let writer = pool.writer().expect("hold writer throughout cancellation");
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let acquiring_runtime = runtime.clone();
        let acquiring = tokio::spawn(scope_request_read_cancellation(
            cancel_rx,
            acquire_store("memory.recall.note_store", move || {
                // This runs after acquire_store's initial context check, so
                // cancellation cannot pass by preventing the closure's entry.
                let _ = entered_tx.send(());
                let result = match store {
                    "notes" => acquiring_runtime.notes(&token).map(|_| ()),
                    "events" => acquiring_runtime.events(&token).map(|_| ()),
                    "vectors" => acquiring_runtime
                        .vectors_for_model(&token, "cancelled-store-model")
                        .map(|_| ()),
                    _ => unreachable!("unknown constructor"),
                };
                let _ = finished_tx.send(());
                result
            }),
        ));

        let entered = tokio::time::timeout(release_bound, entered_rx).await;
        if !matches!(entered, Ok(Ok(()))) {
            drop(writer);
            let _ = acquiring.await;
            panic!("acquisition worker must enter before the stop signal");
        }

        match stop {
            AcquisitionStop::Cancel => cancel_tx.send(true).expect("cancel acquisition"),
            AcquisitionStop::Drop => acquiring.abort(),
        }
        let marker = tokio::task::spawn_blocking(|| ());
        let released = tokio::time::timeout(release_bound, async {
            finished_rx.await.expect("constructor returned");
            marker.await.expect("blocking worker is available again");
        })
        .await;

        // Always release the held writer before asserting a negative result,
        // so a regressed worker can settle without delaying runtime teardown.
        drop(writer);
        let acquired = acquiring.await;
        assert!(
            released.is_ok(),
            "worker must return while writer is held, within {release_bound:?}, before the {checkout_timeout:?} checkout timeout"
        );
        match stop {
            AcquisitionStop::Cancel => {
                let result = acquired
                    .expect("acquisition task joins normally")
                    .and_then(std::convert::identity);
                assert!(
                    matches!(
                        result,
                        Err(RuntimeError::Storage(StorageError::Timeout { .. }))
                    ),
                    "cancelled constructor must preserve the request timeout classification"
                );
            }
            AcquisitionStop::Drop => assert!(
                matches!(acquired, Err(error) if error.is_cancelled()),
                "the async waiter must be dropped"
            ),
        }
        assert_eq!(
            pool.writer_acquisition_snapshot().timeouts,
            before.timeouts,
            "request cancellation must not exhaust the pool checkout budget"
        );
    });
}

#[test]
fn cancelled_store_acquisition_releases_worker_while_writer_is_held() {
    for store in ["notes", "events", "vectors"] {
        stopped_acquisition_releases_the_blocking_worker(AcquisitionStop::Cancel, store);
    }
}

#[test]
fn dropped_store_acquisition_releases_worker_while_writer_is_held() {
    for store in ["notes", "events", "vectors"] {
        stopped_acquisition_releases_the_blocking_worker(AcquisitionStop::Drop, store);
    }
}

#[test]
fn uncancelled_store_acquisition_waits_for_writer_and_succeeds() {
    use std::time::Duration;

    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("single-worker test runtime");
    executor.block_on(async {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let token = runtime
            .authorize(Namespace::local())
            .expect("authorize local");
        // No accessor for this store kind has run on this backend: cold acquisition.
        let pool = runtime.backend().pool();
        let checkout_timeout = pool.config().checkout_timeout;
        let release_bound = Duration::from_secs(1).min(checkout_timeout / 2);
        assert!(release_bound >= Duration::from_millis(100));
        let before = pool.writer_acquisition_snapshot();
        let writer = pool
            .writer()
            .expect("hold writer before ordinary acquisition");
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (finished_tx, mut finished_rx) = tokio::sync::oneshot::channel();
        let acquiring_runtime = runtime.clone();
        let acquiring = tokio::spawn(acquire_store("memory.recall.note_store", move || {
            let _ = entered_tx.send(());
            let result = acquiring_runtime.notes(&token);
            let _ = finished_tx.send(());
            result
        }));
        let entered = tokio::time::timeout(release_bound, entered_rx).await;
        let waiting = tokio::time::timeout(Duration::from_millis(50), &mut finished_rx).await;
        drop(writer);
        let acquired = tokio::time::timeout(release_bound, acquiring).await;

        assert!(matches!(entered, Ok(Ok(()))), "worker must enter");
        assert!(
            waiting.is_err(),
            "uncancelled worker must wait for the held writer"
        );
        acquired
            .expect("acquisition finishes after writer release")
            .expect("acquisition task joins normally")
            .and_then(std::convert::identity)
            .expect("ordinary contention must still acquire its store");
        assert_eq!(pool.writer_acquisition_snapshot().timeouts, before.timeouts);
    });
}

fn warm_acquisition_finishes_with_writer_held(cancellation_scoped: bool) {
    use khive_storage::scope_request_read_cancellation;
    use std::time::Duration;

    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("single-worker runtime");
    executor.block_on(async {
        // Vector model construction has a separate, writer-taking schema path.
        for store in ["notes", "events"] {
            let runtime = KhiveRuntime::memory().expect("runtime");
            let token = runtime.authorize(Namespace::local()).unwrap();
            match store {
                "notes" => runtime.notes(&token).map(|_| ()),
                "events" => runtime.events(&token).map(|_| ()),
                _ => unreachable!(),
            }
            .expect("first real accessor initializes the tested kind");
            let pool = runtime.backend().pool();
            let bound = Duration::from_secs(1).min(pool.config().checkout_timeout / 2);
            assert!(bound >= Duration::from_millis(100));
            let writer = pool.writer().expect("hold writer after warming kind");
            let before = pool.writer_acquisition_snapshot();
            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
            let acquiring_runtime = runtime.clone();
            let mut acquiring = tokio::spawn(async move {
                let future = acquire_store("memory.recall.store", move || match store {
                    "notes" => acquiring_runtime.notes(&token).map(|_| ()),
                    "events" => acquiring_runtime.events(&token).map(|_| ()),
                    _ => unreachable!(),
                });
                if cancellation_scoped {
                    scope_request_read_cancellation(cancel_rx, future).await
                } else {
                    future.await
                }
            });
            let while_held = tokio::time::timeout(bound, &mut acquiring).await;
            // A negative witness still releases the writer and settles its worker.
            drop(writer);
            let result = match while_held {
                Ok(joined) => joined.expect("warm task joins"),
                Err(_) => {
                    let _ = tokio::time::timeout(bound, acquiring).await;
                    panic!("{store}: warm acquisition must finish before writer release");
                }
            };
            result
                .and_then(std::convert::identity)
                .expect("warm accessor succeeds");
            let after = pool.writer_acquisition_snapshot();
            assert_eq!(
                after.timeouts, before.timeouts,
                "{store}: no checkout timeout"
            );
            assert_eq!(
                after.pooled_acquisitions, before.pooled_acquisitions,
                "{store}: warm constructor must not check out a writer"
            );
            // This arm proves completion before any stop signal; it does not
            // waive cancellation admission for an already-cancelled request.
            drop(cancel_tx);
        }
    });
}

#[test]
fn warm_uncancelled_store_acquisition_finishes_while_writer_is_held() {
    warm_acquisition_finishes_with_writer_held(false);
}

#[test]
fn warm_cancellation_scoped_store_acquisition_finishes_while_writer_is_held() {
    warm_acquisition_finishes_with_writer_held(true);
}
