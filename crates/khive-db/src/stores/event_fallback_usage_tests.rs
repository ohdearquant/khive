use super::*;
use crate::pool::PoolConfig;
use khive_storage::usage::{scope, UsageContext};
use serde_json::json;
use std::sync::atomic::Ordering;

struct Fixture {
    store: SqlEventStore,
    pool: Arc<ConnectionPool>,
    _directory: Option<tempfile::TempDir>,
}

impl Fixture {
    fn new(file_backed: bool) -> Self {
        let directory = file_backed.then(|| tempfile::tempdir().unwrap());
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: directory.as_ref().map(|dir| dir.path().join("events.db")),
                write_queue_enabled: Some(false),
                write_routing_strict: false,
                ..PoolConfig::for_test()
            })
            .unwrap(),
        );
        {
            let writer = pool.writer().unwrap();
            writer.conn().execute_batch(EVENTS_DDL).unwrap();
        }
        let store = SqlEventStore::new_scoped(Arc::clone(&pool), file_backed, "local");
        let fixture = Self {
            store,
            pool,
            _directory: directory,
        };
        fixture.assert_fallback();
        fixture
    }

    fn assert_fallback(&self) {
        assert!(!self.pool.write_queue_active());
        assert!(self.pool.writer_task_handle().unwrap().is_none());
    }

    fn lose_next_reply(&self) {
        self.store
            .lose_next_fallback_reply
            .store(true, Ordering::SeqCst);
    }

    async fn assert_committed(&self, events: &[Event]) {
        for event in events {
            assert_eq!(
                self.store.get_event(event.id).await.unwrap().as_ref(),
                Some(event),
                "the lost reply must follow the actual fallback commit"
            );
        }
        assert!(!self.store.lose_next_fallback_reply.load(Ordering::SeqCst));
        self.assert_fallback();
    }
}

fn event() -> Event {
    Event::new(
        "local",
        "search",
        EventKind::SearchExecuted,
        SubstrateKind::Note,
        "agent:test",
    )
    .with_payload(json!({"result_kind": "note"}))
}

fn assert_unknown(error: StorageError) {
    assert!(matches!(
        error,
        StorageError::WriterTaskTerminated {
            request_state: WriterTaskRequestState::SideEffectsUnknown,
            ..
        }
    ));
}

async fn single_append_lost_reply(file_backed: bool) {
    let fixture = Fixture::new(file_backed);
    let context = UsageContext::new();
    let known = event();
    scope(context.clone(), fixture.store.append_event(known.clone()))
        .await
        .unwrap();
    assert_eq!(context.shipping_snapshot(), Some(json!({"event_rows": 1})));

    fixture.lose_next_reply();
    let lost = event();
    let error = scope(context.clone(), fixture.store.append_event(lost.clone()))
        .await
        .unwrap_err();
    assert_unknown(error);
    fixture.assert_committed(&[known, lost]).await;
    assert_eq!(context.snapshot(), json!({"event_rows": 1}));
    assert_eq!(context.shipping_snapshot(), None);
}

async fn batch_append_lost_reply(file_backed: bool) {
    let fixture = Fixture::new(file_backed);
    let context = UsageContext::new();
    let known = vec![event(), event()];
    let summary = scope(context.clone(), fixture.store.append_events(known.clone()))
        .await
        .unwrap();
    assert_eq!(summary.attempted, 2);
    assert_eq!(summary.affected, 2);
    assert_eq!(context.shipping_snapshot(), Some(json!({"event_rows": 2})));

    fixture.lose_next_reply();
    let lost = vec![event(), event(), event()];
    let error = scope(context.clone(), fixture.store.append_events(lost.clone()))
        .await
        .unwrap_err();
    assert_unknown(error);
    fixture.assert_committed(&known).await;
    fixture.assert_committed(&lost).await;
    assert_eq!(context.snapshot(), json!({"event_rows": 2}));
    assert_eq!(context.shipping_snapshot(), None);
}

#[tokio::test]
async fn memory_single_append_lost_reply_withholds_usage_after_commit() {
    single_append_lost_reply(false).await;
}

#[tokio::test]
async fn standalone_single_append_lost_reply_withholds_usage_after_commit() {
    single_append_lost_reply(true).await;
}

#[tokio::test]
async fn memory_batch_append_lost_reply_withholds_usage_after_commit() {
    batch_append_lost_reply(false).await;
}

#[tokio::test]
async fn standalone_batch_append_lost_reply_withholds_usage_after_commit() {
    batch_append_lost_reply(true).await;
}
