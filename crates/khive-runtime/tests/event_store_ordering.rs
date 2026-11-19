use std::sync::Arc;

use khive_runtime::events_split::SplitEventStore;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, StorageBackend};
use khive_storage::{Event, EventFilter, EventStore, PageRequest};
use khive_types::{EventKind, SubstrateKind};
use uuid::Uuid;

const ROWS: [(u128, i64); 6] = [
    (0x10, 20),
    (0xf0, 10),
    (0x90, 30),
    (0x20, 20),
    (0x80, 30),
    (0xa0, 10),
];
const EXPECTED: [u128; 6] = [0x90, 0x80, 0x20, 0x10, 0xf0, 0xa0];
const VERB: &str = "fixture.event_store_ordering";

fn events() -> Vec<Event> {
    ROWS.into_iter()
        .map(|(id, created_at)| {
            let mut event = Event::new(
                "local",
                VERB,
                EventKind::Audit,
                SubstrateKind::Note,
                "actor:fixture",
            );
            event.id = Uuid::from_u128(id);
            event.created_at = created_at;
            event
        })
        .collect()
}

fn filter() -> EventFilter {
    EventFilter {
        verbs: vec![VERB.into()],
        ..EventFilter::default()
    }
}

async fn seed(store: &dyn EventStore, rows: Vec<Event>) {
    let expected = rows.len() as u64;
    let written = store
        .append_events(rows)
        .await
        .expect("seed private event store");
    assert_eq!(written.attempted, expected);
    assert_eq!(written.affected, expected);
    assert_eq!(written.failed, 0, "{written:?}");
}

async fn assert_ordered_pages(store: &dyn EventStore) {
    let expected: Vec<_> = EXPECTED.into_iter().map(Uuid::from_u128).collect();
    assert_eq!(
        store.count_events(filter()).await.unwrap(),
        expected.len() as u64
    );
    let whole = store
        .query_events(
            filter(),
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        whole.items.iter().map(|event| event.id).collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        whole
            .items
            .iter()
            .map(|event| event.created_at)
            .collect::<Vec<_>>(),
        [30, 30, 20, 20, 10, 10]
    );
    for limit in [1_u32, 2, 3, 4] {
        let mut seen = Vec::new();
        for offset in (0..expected.len()).step_by(limit as usize) {
            let page = store
                .query_events(
                    filter(),
                    PageRequest {
                        offset: offset as u64,
                        limit,
                    },
                )
                .await
                .unwrap();
            let ids: Vec<_> = page.items.iter().map(|event| event.id).collect();
            let end = (offset + limit as usize).min(expected.len());
            assert_eq!(ids, expected[offset..end], "offset={offset} limit={limit}");
            seen.extend(ids);
        }
        assert_eq!(seen, expected, "complete pagination at limit={limit}");
        let terminal = store
            .query_events(
                filter(),
                PageRequest {
                    offset: expected.len() as u64,
                    limit,
                },
            )
            .await
            .unwrap();
        assert!(terminal.items.is_empty(), "terminal page at limit={limit}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn attributed_query_events_preserves_created_at_then_id_desc() {
    let backend = Arc::new(StorageBackend::memory().unwrap());
    backend.prepare_core_schema().unwrap();
    let runtime = KhiveRuntime::from_backend(
        backend,
        RuntimeConfig {
            db_path: None,
            events_split: None,
            actor_id: Some("lambda:event-ordering".into()),
            ..RuntimeConfig::no_embeddings()
        },
    );
    let token = runtime.authorize(Namespace::local()).unwrap();
    let store = runtime.events(&token).unwrap();
    let mut rows = events();
    for event in &mut rows {
        event.namespace = "forged".into();
        event.actor = "actor:forged".into();
    }
    seed(store.as_ref(), rows).await;
    let page = store
        .query_events(
            filter(),
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), ROWS.len());
    assert!(page
        .items
        .iter()
        .all(|event| event.namespace == "local" && event.actor == "actor:lambda:event-ordering"));
    assert_ordered_pages(store.as_ref()).await;
}

#[tokio::test(flavor = "current_thread")]
async fn split_query_events_orders_by_created_at_then_id_desc() {
    let legacy_backend = StorageBackend::memory().unwrap();
    let lane_backend = StorageBackend::memory().unwrap();
    let legacy = legacy_backend.events_for_namespace("local").unwrap();
    let lane = lane_backend.events_for_namespace("local").unwrap();
    let rows = events();
    seed(
        legacy.as_ref(),
        [0_usize, 5, 2]
            .into_iter()
            .map(|i| rows[i].clone())
            .collect(),
    )
    .await;
    seed(
        lane.as_ref(),
        [4_usize, 1, 3]
            .into_iter()
            .map(|i| rows[i].clone())
            .collect(),
    )
    .await;
    assert_eq!(legacy.count_events(filter()).await.unwrap(), 3);
    assert_eq!(lane.count_events(filter()).await.unwrap(), 3);
    let split = SplitEventStore::new(legacy, lane);
    assert_ordered_pages(&split).await;
}

#[cfg(unix)]
struct DaemonTask(Option<tokio::task::JoinHandle<anyhow::Result<()>>>);

#[cfg(unix)]
impl Drop for DaemonTask {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

#[cfg(unix)]
impl DaemonTask {
    async fn abort_and_join(mut self) {
        let task = self.0.take().unwrap();
        task.abort();
        let result = task.await;
        assert!(
            matches!(&result, Err(error) if error.is_cancelled()),
            "daemon join: {result:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn forwarding_query_events_preserves_created_at_then_id_desc() {
    use khive_runtime::events_split::{run_events_daemon, EventsSplitClient, ForwardingEventStore};
    use khive_storage::event::EventAppendDisposition;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    // A short private directory avoids Unix socket path limits on macOS.
    let dir = tempfile::Builder::new()
        .prefix("evord-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.path().join("events.sock");
    let database = dir.path().join("events.db");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async {
            let daemon_socket = socket.clone();
            let daemon_database = database.clone();
            let daemon = DaemonTask(Some(tokio::spawn(async move {
                run_events_daemon(&daemon_database, &daemon_socket).await
            })));
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    assert!(
                        !daemon.0.as_ref().unwrap().is_finished(),
                        "events daemon exited before readiness"
                    );
                    if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("events daemon must accept connections");
            let client = EventsSplitClient::new(socket.clone()).unwrap();
            let store = ForwardingEventStore::new("local", client);
            // Plain forwarded append is queued; this acknowledged route removes
            // persistence races from the order assertions.
            let seeded = store.append_events_idempotent(events()).await.unwrap();
            assert_eq!(
                seeded.rows,
                vec![EventAppendDisposition::Inserted; ROWS.len()]
            );
            assert_ordered_pages(&store).await;
            drop(store);
            daemon.abort_and_join().await;
        })
    }));
    // Cancel connection/forwarder tasks before removing their temporary files,
    // including when an order assertion unwinds.
    runtime.shutdown_timeout(Duration::from_secs(2));
    dir.close().unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
