use super::*;
use crate::connector::{TelegramChat, TelegramMessage, TelegramUser};
use std::sync::Mutex as StdMutex;

fn make_config() -> TelegramChannelConfig {
    TelegramChannelConfig {
        bot_token: "test-token".to_string(), // gitleaks:allow
        maintainer_chat_id: 555,
        authorized_sender_id: 555,
        maintainer_slug: "maintainer".to_string(),
        ingest_namespace: "local".to_string(),
    }
}

/// A mock Bot API connector: no live network, just recorded calls and
/// scripted `getUpdates` responses.
struct MockConnector {
    sent: StdMutex<Vec<(i64, String)>>,
    updates: StdMutex<Vec<Vec<TelegramUpdate>>>,
    offsets_seen: StdMutex<Vec<Option<i64>>>,
    fail_send: bool,
}

impl MockConnector {
    fn new(pages: Vec<Vec<TelegramUpdate>>) -> Self {
        Self {
            sent: StdMutex::new(Vec::new()),
            updates: StdMutex::new(pages),
            offsets_seen: StdMutex::new(Vec::new()),
            fail_send: false,
        }
    }

    fn failing_send() -> Self {
        Self {
            sent: StdMutex::new(Vec::new()),
            updates: StdMutex::new(Vec::new()),
            offsets_seen: StdMutex::new(Vec::new()),
            fail_send: true,
        }
    }
}

#[async_trait]
impl TelegramConnector for MockConnector {
    async fn send_message(&self, chat_id: i64, text: &str) -> Result<(), ChannelError> {
        if self.fail_send {
            return Err(ChannelError::Transport("mock send failure".to_string()));
        }
        self.sent.lock().unwrap().push((chat_id, text.to_string()));
        Ok(())
    }

    async fn get_updates(&self, offset: Option<i64>) -> Result<Vec<TelegramUpdate>, ChannelError> {
        self.offsets_seen.lock().unwrap().push(offset);
        let mut pages = self.updates.lock().unwrap();
        if pages.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(pages.remove(0))
        }
    }
}

fn text_update(update_id: i64, chat_id: i64, text: &str, date: i64) -> TelegramUpdate {
    TelegramUpdate {
        update_id,
        message: Some(TelegramMessage {
            message_id: update_id,
            date,
            chat: TelegramChat { id: chat_id },
            from: Some(TelegramUser { id: chat_id }),
            text: Some(text.to_string()),
        }),
    }
}

#[tokio::test]
async fn dropping_inflight_poll_preserves_confirmed_and_pending_offsets() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    struct ParkFirst {
        inner: MockConnector,
        entered: tokio::sync::Notify,
        first: AtomicBool,
        drops: AtomicUsize,
        offsets: StdMutex<Vec<Option<i64>>>,
    }
    #[async_trait]
    impl TelegramConnector for ParkFirst {
        async fn send_message(&self, _: i64, _: &str) -> Result<(), ChannelError> {
            panic!("poll fixture never sends");
        }
        async fn get_updates(
            &self,
            offset: Option<i64>,
        ) -> Result<Vec<TelegramUpdate>, ChannelError> {
            self.offsets.lock().unwrap().push(offset);
            if self.first.swap(false, Ordering::SeqCst) {
                struct OnDrop<'a>(&'a AtomicUsize);
                impl Drop for OnDrop<'_> {
                    fn drop(&mut self) {
                        self.0.fetch_add(1, Ordering::SeqCst);
                    }
                }
                let _drop = OnDrop(&self.drops);
                self.entered.notify_one();
                std::future::pending::<()>().await;
            }
            self.inner.get_updates(offset).await
        }
    }
    let connector = Arc::new(ParkFirst {
        inner: MockConnector::new(vec![
            vec![text_update(10, 555, "cancelled poll replay", 1_700_000_000)],
            vec![],
        ]),
        entered: tokio::sync::Notify::new(),
        first: AtomicBool::new(true),
        drops: AtomicUsize::new(0),
        offsets: StdMutex::new(Vec::new()),
    });
    let channel = TelegramChannel::with_connector(make_config(), Box::new(connector.clone()));
    channel.advance_offset(7);
    // A previous fetched page can remain uncommitted after ingestion fails.
    *channel.pending_offset.lock().unwrap() = Some(9);
    {
        let poll = channel.poll(Utc::now());
        tokio::pin!(poll);
        tokio::select! {
            biased;
            _ = connector.entered.notified() => {},
            _ = &mut poll => panic!("the first request must remain pending"),
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                panic!("connector never entered its pending request")
            }
        }
    }
    assert_eq!(connector.drops.load(Ordering::SeqCst), 1);
    assert_eq!(channel.current_offset(), Some(7));
    assert_eq!(*channel.pending_offset.lock().unwrap(), Some(9));
    let replay = channel.poll(Utc::now()).await.unwrap();
    assert_eq!(replay.len(), 1);
    assert_eq!(replay[0].external_id.as_deref(), Some("tg:555:10"));
    assert_eq!(channel.current_offset(), Some(7));
    channel.commit_offset();
    assert_eq!(channel.current_offset(), Some(11));
    assert!(channel.poll(Utc::now()).await.unwrap().is_empty());
    assert_eq!(
        *connector.offsets.lock().unwrap(),
        vec![Some(7), Some(7), Some(11)]
    );
}

#[test]
fn kind_is_telegram() {
    let ch = TelegramChannel::with_connector(make_config(), Box::new(MockConnector::new(vec![])));
    assert_eq!(ch.kind(), "telegram");
}

#[tokio::test]
async fn poll_authorized_chat_produces_envelope_with_correct_external_id() {
    let updates = vec![vec![text_update(10, 555, "hello khive", 1_700_000_000)]];
    let ch = TelegramChannel::with_connector(make_config(), Box::new(MockConnector::new(updates)));

    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(envs[0].external_id.as_deref(), Some("tg:555:10"));
    assert_eq!(envs[0].from, "telegram:maintainer");
    assert_eq!(envs[0].content, "hello khive");
}

#[tokio::test]
async fn poll_unauthorized_chat_is_dropped_not_returned_as_error() {
    let updates = vec![vec![text_update(
        11,
        999,
        "i am not the maintainer",
        1_700_000_000,
    )]];
    let ch = TelegramChannel::with_connector(make_config(), Box::new(MockConnector::new(updates)));

    let envs = ch.poll(Utc::now()).await.unwrap();
    assert!(
        envs.is_empty(),
        "unauthorized chat id must be dropped with no note, not surfaced as an error"
    );
}

#[tokio::test]
async fn group_chat_accepts_only_configured_sender_and_skips_missing_sender() {
    let mut config = make_config();
    config.maintainer_chat_id = -100123;
    config.authorized_sender_id = 42;
    let mut other = text_update(12, -100123, "other", 1_700_000_000);
    other.message.as_mut().unwrap().from = Some(TelegramUser { id: 99 });
    let mut missing = text_update(13, -100123, "missing", 1_700_000_001);
    missing.message.as_mut().unwrap().from = None;
    let mut authorized = text_update(14, -100123, "authorized", 1_700_000_002);
    authorized.message.as_mut().unwrap().from = Some(TelegramUser { id: 42 });
    let ch = TelegramChannel::with_connector(
        config,
        Box::new(MockConnector::new(vec![vec![other, missing, authorized]])),
    );

    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(envs[0].from, "telegram:maintainer");
    assert_eq!(envs[0].content, "authorized");
    assert_eq!(envs[0].external_id.as_deref(), Some("tg:-100123:14"));
    ch.commit_offset();
    assert_eq!(ch.current_offset(), Some(15));
}

#[tokio::test]
async fn poll_bad_message_in_batch_does_not_abort_others() {
    let updates = vec![vec![
        text_update(20, 999, "attacker", 1_700_000_000),
        text_update(21, 555, "maintainer says hi", 1_700_000_001),
    ]];
    let ch = TelegramChannel::with_connector(make_config(), Box::new(MockConnector::new(updates)));

    let envs = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(envs.len(), 1, "only the authorized update must be returned");
    assert_eq!(envs[0].external_id.as_deref(), Some("tg:555:21"));
}

#[tokio::test]
async fn poll_advances_offset_past_all_fetched_updates_including_unauthorized_after_commit() {
    let updates = vec![
        vec![
            text_update(1, 999, "attacker", 1_700_000_000),
            text_update(2, 555, "maintainer", 1_700_000_001),
        ],
        vec![],
    ];
    let connector = std::sync::Arc::new(MockConnector::new(updates));
    let ch = TelegramChannel::with_connector(make_config(), Box::new(connector.clone()));

    let first = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(first.len(), 1);
    ch.commit_offset();
    let _second = ch.poll(Utc::now()).await.unwrap();

    let seen = connector.offsets_seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![None, Some(3)],
        "second poll must request offset = last update_id + 1, once committed"
    );
}

#[tokio::test]
async fn poll_without_commit_reuses_old_offset_so_ingest_failure_can_retry() {
    // Same batch returned twice: Telegram keeps re-delivering it because
    // the offset was never acknowledged (no `commit_offset()` call
    // between the two polls), simulating a failed `comm.ingest`.
    let batch = vec![text_update(9, 555, "maintainer", 1_700_000_000)];
    let updates = vec![batch.clone(), batch];
    let connector = std::sync::Arc::new(MockConnector::new(updates));
    let ch = TelegramChannel::with_connector(make_config(), Box::new(connector.clone()));

    let first = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(first.len(), 1);
    // Deliberately no commit_offset() call here — simulates comm.ingest
    // failing for this batch.
    let second = ch.poll(Utc::now()).await.unwrap();
    assert_eq!(
        second.len(),
        1,
        "an uncommitted batch must be re-fetched and re-delivered, not lost"
    );

    let seen = connector.offsets_seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![None, None],
        "retry without commit must reuse the old (unset) offset"
    );
}

#[tokio::test]
async fn commit_offset_after_successful_retry_advances_offset() {
    let batch = vec![text_update(9, 555, "maintainer", 1_700_000_000)];
    let updates = vec![batch.clone(), batch, vec![]];
    let connector = std::sync::Arc::new(MockConnector::new(updates));
    let ch = TelegramChannel::with_connector(make_config(), Box::new(connector.clone()));

    let _first = ch.poll(Utc::now()).await.unwrap(); // simulated ingest failure, no commit
    let _retry = ch.poll(Utc::now()).await.unwrap(); // re-fetched same batch
    ch.commit_offset(); // this time ingest succeeds
    let _third = ch.poll(Utc::now()).await.unwrap();

    let seen = connector.offsets_seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![None, None, Some(10)],
        "offset only advances after commit_offset() following a successful ingest"
    );
}

#[tokio::test]
async fn commit_offset_without_pending_poll_is_a_noop() {
    let ch = TelegramChannel::with_connector(make_config(), Box::new(MockConnector::new(vec![])));
    ch.commit_offset();
    ch.commit_offset();
    assert_eq!(ch.current_offset(), None);
}

#[tokio::test]
async fn send_to_maintainer_slug_is_routed() {
    let connector = MockConnector::new(vec![]);
    let ch = TelegramChannel::with_connector(make_config(), Box::new(connector));

    let env = ChannelEnvelope::new(BOT_SELF_ADDRESS, "telegram:maintainer", "reply text");
    ch.send(env).await.unwrap();
}

#[tokio::test]
async fn send_to_unroutable_slug_is_permanent_and_not_misdelivered() {
    let connector = std::sync::Arc::new(MockConnector::new(vec![]));
    let ch = TelegramChannel::with_connector(make_config(), Box::new(connector.clone()));

    let env = ChannelEnvelope::new(BOT_SELF_ADDRESS, "telegram:someone-else", "reply text");
    let error = ch.send(env).await.expect_err("unroutable slug must fail");

    assert!(
        connector.sent.lock().unwrap().is_empty(),
        "an unroutable slug must never be sent to the maintainer chat"
    );
    assert!(matches!(error, ChannelError::InvalidEnvelope(_)));
    assert_eq!(
        error.delivery_failure_class(),
        khive_channel::DeliveryFailureClass::Permanent
    );
}

#[tokio::test]
async fn send_propagates_connector_error() {
    let ch =
        TelegramChannel::with_connector(make_config(), Box::new(MockConnector::failing_send()));
    let env = ChannelEnvelope::new(BOT_SELF_ADDRESS, "telegram:maintainer", "reply text");
    let err = ch.send(env).await.unwrap_err();
    assert!(matches!(err, ChannelError::Transport(_)));
}

#[tokio::test]
async fn poll_message_without_text_is_dropped() {
    let update = TelegramUpdate {
        update_id: 5,
        message: Some(TelegramMessage {
            message_id: 5,
            date: 1_700_000_000,
            chat: TelegramChat { id: 555 },
            from: Some(TelegramUser { id: 555 }),
            text: None,
        }),
    };
    let ch = TelegramChannel::with_connector(
        make_config(),
        Box::new(MockConnector::new(vec![vec![update]])),
    );
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert!(
        envs.is_empty(),
        "a non-text update must be dropped, not errored"
    );
}

#[tokio::test]
async fn poll_update_without_message_is_dropped() {
    let update = TelegramUpdate {
        update_id: 6,
        message: None,
    };
    let ch = TelegramChannel::with_connector(
        make_config(),
        Box::new(MockConnector::new(vec![vec![update]])),
    );
    let envs = ch.poll(Utc::now()).await.unwrap();
    assert!(envs.is_empty());
}

#[test]
fn is_configured_true_with_nonempty_token() {
    let ch = TelegramChannel::with_connector(make_config(), Box::new(MockConnector::new(vec![])));
    assert!(ch.is_configured());
}

#[test]
fn strip_kind_prefix_removes_prefix() {
    assert_eq!(
        strip_kind_prefix("telegram:maintainer", "telegram"),
        "maintainer"
    );
    assert_eq!(strip_kind_prefix("maintainer", "telegram"), "maintainer");
}

#[test]
fn maintainer_slug_accessor() {
    let ch = TelegramChannel::with_connector(make_config(), Box::new(MockConnector::new(vec![])));
    assert_eq!(ch.maintainer_slug(), "maintainer");
}
