use super::*;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use khive_channel::{Channel, ChannelEnvelope, ChannelError};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

struct ParkedChannel {
    kind: &'static str,
    entered: Notify,
    calls: AtomicUsize,
    drops: AtomicUsize,
    ready: AtomicBool,
    delivered: AtomicBool,
    #[cfg(feature = "channel-telegram")]
    commits: AtomicUsize,
    checkpoints: Mutex<Vec<Option<u64>>>,
}

impl ParkedChannel {
    fn new(kind: &'static str) -> Self {
        Self {
            kind,
            entered: Notify::new(),
            calls: AtomicUsize::new(0),
            drops: AtomicUsize::new(0),
            ready: AtomicBool::new(false),
            delivered: AtomicBool::new(false),
            #[cfg(feature = "channel-telegram")]
            commits: AtomicUsize::new(0),
            checkpoints: Mutex::new(Vec::new()),
        }
    }

    async fn fetch(&self) -> Vec<ChannelEnvelope> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.ready.load(Ordering::SeqCst) && !self.delivered.swap(true, Ordering::SeqCst) {
            return vec![ChannelEnvelope::new(
                format!("{}:sender", self.kind),
                format!("{}:receiver", self.kind),
                "poll cancellation fixture",
            )
            .with_external_id(format!("{}:cancel-fixture:1", self.kind))];
        }
        struct OnDrop<'a>(&'a AtomicUsize);
        impl Drop for OnDrop<'_> {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let _drop = OnDrop(&self.drops);
        self.entered.notify_one();
        std::future::pending().await
    }
}

#[async_trait]
impl Channel for ParkedChannel {
    fn kind(&self) -> &'static str {
        self.kind
    }

    async fn send(&self, _: ChannelEnvelope) -> Result<(), ChannelError> {
        panic!("inbound fixture must never send");
    }

    async fn poll(&self, _: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
        Ok(self.fetch().await)
    }

    async fn poll_page(
        &self,
        _: DateTime<Utc>,
        checkpoint: Option<&khive_channel::StoredChannelCheckpoint>,
    ) -> Result<khive_channel::ChannelPollPage, ChannelError> {
        self.checkpoints
            .lock()
            .unwrap()
            .push(checkpoint.and_then(|cp| cp.checkpoint.high_water));
        Ok(khive_channel::ChannelPollPage {
            envelopes: self.fetch().await,
            next_checkpoint: Some(khive_channel::ChannelCheckpoint {
                source: "cancellation-fixture".into(),
                generation: 1,
                high_water: Some(1),
            }),
        })
    }
}

#[cfg(feature = "channel-telegram")]
impl TelegramPollChannel for ParkedChannel {
    fn commit_offset(&self) {
        self.commits.fetch_add(1, Ordering::SeqCst);
    }
}

fn registry() -> khive_runtime::VerbRegistry {
    let runtime = khive_runtime::KhiveRuntime::memory().unwrap();
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    khive_runtime::PackRegistry::register_packs(
        &["kg".to_owned(), "comm".to_owned()],
        runtime,
        &mut builder,
    )
    .unwrap();
    builder.build().unwrap()
}

async fn message_count(registry: &khive_runtime::VerbRegistry) -> usize {
    registry
        .dispatch(
            "list",
            serde_json::json!({"namespace": "local", "kind": "message", "limit": 50}),
        )
        .await
        .unwrap()["items"]
        .as_array()
        .unwrap()
        .len()
}

async fn parked(channel: &ParkedChannel, task: &mut tokio::task::JoinHandle<()>) {
    if tokio::time::timeout(Duration::from_secs(20), channel.entered.notified())
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
        panic!("the real poll loop must enter the pending request");
    }
}

async fn cancel_and_join(token: CancellationToken, mut task: tokio::task::JoinHandle<()>) {
    token.cancel();
    match tokio::time::timeout(Duration::from_secs(5), &mut task).await {
        Ok(result) => result.expect("poll task must not panic"),
        Err(_) => {
            task.abort();
            let _ = task.await;
            panic!("shutdown must drop the parked request before the 10s daemon drain budget");
        }
    }
}

#[cfg(feature = "channel-email")]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn email_inflight_poll_cancels_without_committing_cursor_and_restart_ingests_once() {
    let registry = registry();
    let channel = Arc::new(ParkedChannel::new("mock_cancel"));
    let mut channels = khive_channel::ChannelRegistry::new();
    channels.register(channel.clone());
    let channels = Arc::new(channels);
    let spawn = |token| {
        tokio::spawn(channel_poll_loop(
            channels.clone(),
            registry.clone(),
            "local".into(),
            "actor:test".into(),
            token,
        ))
    };

    let token = CancellationToken::new();
    let mut task = spawn(token.clone());
    parked(&channel, &mut task).await;
    cancel_and_join(token, task).await;
    assert_eq!(channel.calls.load(Ordering::SeqCst), 1);
    assert_eq!(channel.drops.load(Ordering::SeqCst), 1);
    assert_eq!(message_count(&registry).await, 0);
    assert!(load_channel_cursor(&registry, channel.kind, channel.kind)
        .await
        .unwrap()
        .is_none());

    channel.ready.store(true, Ordering::SeqCst);
    let token = CancellationToken::new();
    let mut task = spawn(token.clone());
    parked(&channel, &mut task).await;
    cancel_and_join(token, task).await;
    assert_eq!(channel.calls.load(Ordering::SeqCst), 3);
    assert_eq!(channel.drops.load(Ordering::SeqCst), 2);
    assert_eq!(message_count(&registry).await, 1);
    assert_eq!(
        *channel.checkpoints.lock().unwrap(),
        vec![None, None, Some(1)],
        "only the completed and durably ingested page may advance its checkpoint"
    );
    assert_eq!(
        load_channel_cursor(&registry, channel.kind, channel.kind)
            .await
            .unwrap()
            .unwrap()
            .checkpoint
            .high_water,
        Some(1)
    );
}

#[cfg(feature = "channel-telegram")]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn telegram_inflight_poll_cancels_without_ack_and_restart_ingests_once() {
    let registry = registry();
    let channel = Arc::new(ParkedChannel::new("telegram"));
    let spawn = |token| {
        tokio::spawn(telegram_poll_loop(
            channel.clone(),
            registry.clone(),
            "local".into(),
            "local".into(),
            token,
        ))
    };
    let token = CancellationToken::new();
    let mut task = spawn(token.clone());
    parked(&channel, &mut task).await;
    cancel_and_join(token, task).await;
    assert_eq!(channel.calls.load(Ordering::SeqCst), 1);
    assert_eq!(channel.drops.load(Ordering::SeqCst), 1);
    assert_eq!(channel.commits.load(Ordering::SeqCst), 0);
    assert_eq!(message_count(&registry).await, 0);

    channel.ready.store(true, Ordering::SeqCst);
    let token = CancellationToken::new();
    let mut task = spawn(token.clone());
    parked(&channel, &mut task).await;
    cancel_and_join(token, task).await;
    assert_eq!(channel.calls.load(Ordering::SeqCst), 3);
    assert_eq!(channel.drops.load(Ordering::SeqCst), 2);
    assert_eq!(channel.commits.load(Ordering::SeqCst), 1);
    assert_eq!(message_count(&registry).await, 1);
}
