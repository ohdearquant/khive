use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Boundary {
    #[cfg(feature = "channel-email")]
    EmailBeforePoll,
    #[cfg(feature = "channel-email")]
    EmailBeforeCommit,
    #[cfg(feature = "test-channel-timing")]
    TelegramBeforePoll,
    #[cfg(feature = "test-channel-timing")]
    TelegramBeforeCommit,
}

struct Gate {
    boundary: Boundary,
    entered: Notify,
    release: Semaphore,
    arrivals: AtomicUsize,
}

impl Gate {
    fn new(boundary: Boundary) -> Arc<Self> {
        Arc::new(Self {
            boundary,
            entered: Notify::new(),
            release: Semaphore::new(0),
            arrivals: AtomicUsize::new(0),
        })
    }
}

tokio::task_local! {
    static ACTIVE: Arc<Gate>;
}

pub(super) async fn at(boundary: Boundary) {
    let Ok(gate) = ACTIVE.try_with(Arc::clone) else {
        return;
    };
    if gate.boundary == boundary {
        gate.arrivals.fetch_add(1, Ordering::SeqCst);
        gate.entered.notify_one();
        gate.release
            .acquire()
            .await
            .expect("fixture release remains open")
            .forget();
    }
}

async fn reach(gate: &Gate, task: &mut tokio::task::JoinHandle<()>) {
    if tokio::time::timeout(Duration::from_secs(20), gate.entered.notified())
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
        panic!("actual coordinator did not reach the declared timing boundary");
    }
}

async fn finish(mut task: tokio::task::JoinHandle<()>) {
    match tokio::time::timeout(Duration::from_secs(10), &mut task).await {
        Ok(result) => result.expect("coordinator must not panic"),
        Err(_) => {
            task.abort();
            let _ = task.await;
            panic!("released coordinator did not finish; not an intended semantic failure");
        }
    }
}

fn registry() -> khive_runtime::VerbRegistry {
    let runtime = khive_runtime::KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    khive_runtime::PackRegistry::register_packs(
        &["kg".to_string(), "comm".to_string()],
        runtime,
        &mut builder,
    )
    .expect("real kg and comm packs");
    builder.build().expect("real registry")
}

async fn messages(
    registry: &khive_runtime::VerbRegistry,
) -> Result<serde_json::Value, khive_runtime::RuntimeError> {
    registry
        .dispatch(
            "list",
            serde_json::json!({
                "namespace": "local", "kind": "message", "limit": 50,
            }),
        )
        .await
        .map(|value| value["items"].clone())
}

async fn observe<T>(
    future: impl std::future::Future<Output = T>,
    task: &mut tokio::task::JoinHandle<()>,
) -> T {
    match tokio::time::timeout(Duration::from_secs(20), future).await {
        Ok(value) => value,
        Err(_) => {
            task.abort();
            let _ = task.await;
            panic!("parked-state observation timed out; not an intended semantic failure");
        }
    }
}

#[cfg(feature = "channel-email")]
mod email {
    use super::*;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use khive_channel::{
        Channel, ChannelCheckpoint, ChannelEnvelope, ChannelError, ChannelPollPage,
        StoredChannelCheckpoint,
    };

    const KIND: &str = "mock_timing";
    const SOURCE: &str = "timing-fixture";

    struct ReadyPage {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Channel for ReadyPage {
        fn kind(&self) -> &'static str {
            KIND
        }
        async fn send(&self, _: ChannelEnvelope) -> Result<(), ChannelError> {
            panic!("inbound fixture must not send");
        }
        async fn poll(&self, _: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
            panic!("email coordinator must use poll_page");
        }
        async fn poll_page(
            &self,
            _: DateTime<Utc>,
            _: Option<&StoredChannelCheckpoint>,
        ) -> Result<ChannelPollPage, ChannelError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ChannelPollPage {
                envelopes: vec![ChannelEnvelope::new(
                    "mock_timing:sender",
                    "mock_timing:receiver",
                    "timing selected page",
                )
                .with_external_id("timing-page:9")],
                next_checkpoint: Some(checkpoint(9)),
            })
        }
    }

    fn checkpoint(high_water: u64) -> ChannelCheckpoint {
        ChannelCheckpoint {
            source: SOURCE.into(),
            generation: 1,
            high_water: Some(high_water),
        }
    }

    async fn run(boundary: Boundary, expect_commit: bool) {
        let registry = registry();
        commit_channel_cursor(&registry, KIND, KIND, &checkpoint(7))
            .await
            .expect("seed actual checkpoint");
        let initial = load_channel_cursor(&registry, KIND, KIND)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(initial.checkpoint, checkpoint(7));
        assert_eq!(messages(&registry).await.unwrap(), serde_json::json!([]));
        let channel = Arc::new(ReadyPage {
            calls: AtomicUsize::new(0),
        });
        let mut channels = khive_channel::ChannelRegistry::new();
        channels.register(channel.clone());
        let token = CancellationToken::new();
        let gate = Gate::new(boundary);
        let mut task = tokio::spawn(ACTIVE.scope(
            gate.clone(),
            channel_poll_loop(
                Arc::new(channels),
                registry.clone(),
                "local".into(),
                "actor:test".into(),
                token.clone(),
            ),
        ));
        reach(&gate, &mut task).await;
        // Observe while the exact loop-owned boundary is parked; assertions follow cleanup.
        let (parked_checkpoint, parked_messages) = observe(
            async {
                (
                    load_channel_cursor(&registry, KIND, KIND).await,
                    messages(&registry).await,
                )
            },
            &mut task,
        )
        .await;
        let parked_calls = channel.calls.load(Ordering::SeqCst);
        let cancelled_before = token.is_cancelled();
        token.cancel();
        gate.release.add_permits(1);
        finish(task).await;
        assert!(
            !cancelled_before,
            "cancellation must follow the actual boundary arrival"
        );
        assert_eq!(gate.arrivals.load(Ordering::SeqCst), 1);
        assert_eq!(parked_checkpoint.unwrap().unwrap(), initial);
        let parked_messages = parked_messages.unwrap();
        assert_eq!(parked_calls, usize::from(expect_commit));
        assert_eq!(
            parked_messages.as_array().unwrap().len(),
            usize::from(expect_commit)
        );
        if expect_commit {
            assert_eq!(
                parked_messages[0]["properties"]["external_id"],
                "timing-page:9"
            );
        }
        let final_checkpoint = load_channel_cursor(&registry, KIND, KIND)
            .await
            .unwrap()
            .unwrap();
        let final_messages = messages(&registry).await.unwrap();
        eprintln!("email timing controls passed: {boundary:?}; existing checkpoint and real ingest observed");
        assert_eq!(
            final_checkpoint.checkpoint,
            checkpoint(if expect_commit { 9 } else { 7 }),
            "email timing checkpoint boundary"
        );
        assert_eq!(
            final_messages.as_array().unwrap().len(),
            usize::from(expect_commit),
            "email timing persisted rows"
        );
        assert_eq!(
            channel.calls.load(Ordering::SeqCst),
            usize::from(expect_commit),
            "cancel-first must not poll the ready page"
        );
        if expect_commit {
            assert_eq!(
                final_messages, parked_messages,
                "completed ingest survives cancellation without duplication"
            );
        } else {
            assert_eq!(
                final_checkpoint, initial,
                "cancel-first preserves the full stored checkpoint including commit time"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn both_ready_cancel_wins_before_transport_selection() {
        run(Boundary::EmailBeforePoll, false).await;
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn cancellation_at_selected_page_commit_finishes_checkpoint() {
        run(Boundary::EmailBeforeCommit, true).await;
    }
}

#[cfg(feature = "test-channel-timing")]
mod telegram {
    use super::*;
    use chrono::Utc;
    use khive_channel::Channel;
    use khive_channel_telegram::test_support::TelegramTimingFixture;

    async fn run(boundary: Boundary, expect_commit: bool) {
        let fixture = TelegramTimingFixture::ready();
        let channel = fixture.channel();
        assert_eq!(channel.poll(Utc::now()).await.unwrap().len(), 1);
        channel.commit_offset();
        assert_eq!(channel.poll(Utc::now()).await.unwrap().len(), 1);
        assert_eq!(
            fixture.offsets(),
            (Some(7), Some(9)),
            "prime with actual poll/commit methods"
        );
        assert_eq!(fixture.requested_offsets(), vec![None, Some(7)]);
        assert_eq!(
            fixture.remaining_pages(),
            1,
            "the third transport response is immediately ready"
        );
        let registry = registry();
        assert_eq!(messages(&registry).await.unwrap(), serde_json::json!([]));
        let token = CancellationToken::new();
        let gate = Gate::new(boundary);
        let mut task = tokio::spawn(ACTIVE.scope(
            gate.clone(),
            telegram_poll_loop(channel, registry.clone(), "local".into(), token.clone()),
        ));
        reach(&gate, &mut task).await;
        let parked_offsets = fixture.offsets();
        let parked_requests = fixture.requested_offsets();
        let parked_pages = fixture.remaining_pages();
        let parked_messages = observe(messages(&registry), &mut task).await;
        let cancelled_before = token.is_cancelled();
        token.cancel();
        gate.release.add_permits(1);
        finish(task).await;
        assert!(
            !cancelled_before,
            "cancellation must follow actual coordinator arrival"
        );
        assert_eq!(gate.arrivals.load(Ordering::SeqCst), 1);
        assert_eq!(
            parked_offsets,
            (Some(7), Some(if expect_commit { 11 } else { 9 }))
        );
        assert_eq!(
            parked_requests,
            if expect_commit {
                vec![None, Some(7), Some(7)]
            } else {
                vec![None, Some(7)]
            }
        );
        assert_eq!(parked_pages, usize::from(!expect_commit));
        let parked_messages = parked_messages.unwrap();
        assert_eq!(
            parked_messages.as_array().unwrap().len(),
            usize::from(expect_commit)
        );
        if expect_commit {
            assert_eq!(parked_messages[0]["properties"]["external_id"], "tg:555:10");
        }
        let final_messages = messages(&registry).await.unwrap();
        eprintln!("telegram timing controls passed: {boundary:?}; actual adapter offsets and real ingest observed");
        assert_eq!(
            fixture.offsets(),
            if expect_commit {
                (Some(11), None)
            } else {
                (Some(7), Some(9))
            },
            "telegram timing offset boundary"
        );
        assert_eq!(
            fixture.requested_offsets(),
            parked_requests,
            "cancel-first must not issue the ready poll"
        );
        assert_eq!(fixture.remaining_pages(), parked_pages);
        assert_eq!(
            final_messages, parked_messages,
            "selected ingest survives cancellation; tie adds no rows"
        );
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn both_ready_cancel_wins_with_real_adapter_offsets() {
        run(Boundary::TelegramBeforePoll, false).await;
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn cancellation_at_selected_batch_commit_finishes_real_offset() {
        run(Boundary::TelegramBeforeCommit, true).await;
    }
}
