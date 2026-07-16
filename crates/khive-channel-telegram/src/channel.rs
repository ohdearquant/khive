//! `TelegramChannel` — implements the `Channel` trait for the Telegram Bot API
//! (ADR-056 Amendment 2026-07-05).

use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use khive_channel::{Channel, ChannelEnvelope, ChannelError};

use crate::config::TelegramChannelConfig;
use crate::connector::{LiveTelegramConnector, TelegramConnector, TelegramUpdate};

/// Self-identifying address used as the `to`/`from` half of an envelope that
/// represents the bot's own end of the chat. The ADR defines only the
/// maintainer's `telegram:<slug>` address; there is no separate configured
/// identity for the bot side, so a fixed marker is used (mirrors the email
/// adapter using its own configured mailbox address for the analogous slot).
const BOT_SELF_ADDRESS: &str = "telegram:bot";

/// Telegram channel adapter implementing `Channel` via the Bot API
/// `sendMessage`/`getUpdates` methods.
///
/// Configuration is provided at construction time via `TelegramChannelConfig`.
/// The `getUpdates` offset watermark is held in memory only (ADR-056
/// Amendment 2026-07-05, "Poll offset and restart durability") — restart
/// durability relies on Telegram's own server-side offset semantics plus the
/// `idx_comm_message_external_id` dedup index, not on a persisted cursor.
pub struct TelegramChannel {
    config: TelegramChannelConfig,
    connector: Box<dyn TelegramConnector>,
    offset: Mutex<Option<i64>>,
}

impl TelegramChannel {
    /// Build from environment variables. Fails if any required env var is absent.
    pub fn from_env() -> Result<Self, ChannelError> {
        let config = TelegramChannelConfig::from_env()?;
        let connector = LiveTelegramConnector::new(config.bot_token.clone());
        Ok(Self::with_connector(config, Box::new(connector)))
    }

    /// Build from a pre-constructed connector (for testing against a mock
    /// Bot API — never a live network call in unit tests).
    pub(crate) fn with_connector(
        config: TelegramChannelConfig,
        connector: Box<dyn TelegramConnector>,
    ) -> Self {
        Self {
            config,
            connector,
            offset: Mutex::new(None),
        }
    }

    /// The slug this channel routes outbound `telegram:<slug>` sends to.
    pub fn maintainer_slug(&self) -> &str {
        &self.config.maintainer_slug
    }

    fn current_offset(&self) -> Option<i64> {
        *self.offset.lock().expect("offset mutex is never poisoned")
    }

    fn advance_offset(&self, next: i64) {
        let mut guard = self.offset.lock().expect("offset mutex is never poisoned");
        if guard.map(|o| next > o).unwrap_or(true) {
            *guard = Some(next);
        }
    }

    /// Convert one Telegram update into a `ChannelEnvelope`, or `None` when
    /// the update is not an authenticated, actionable text message (ADR-056
    /// §8: unauthorized-sender updates and non-text/non-message updates are
    /// dropped with no note written, never surfaced as an error that would
    /// abort the rest of the batch).
    fn envelope_for(&self, update: &TelegramUpdate) -> Option<ChannelEnvelope> {
        let message = update.message.as_ref()?;

        if message.chat.id != self.config.maintainer_chat_id {
            tracing::warn!(
                chat_id = message.chat.id,
                update_id = update.update_id,
                "telegram: update from unauthorized chat id, dropping"
            );
            return None;
        }

        let text = message.text.as_ref()?;
        let external_id = format!("tg:{}:{}", message.chat.id, update.update_id);
        let from = format!("telegram:{}", self.config.maintainer_slug);
        let sent_at = Utc.timestamp_opt(message.date, 0).single();

        let mut env = ChannelEnvelope::new(from, BOT_SELF_ADDRESS, text.clone())
            .with_external_id(external_id);
        if let Some(ts) = sent_at {
            env = env.with_sent_at(ts);
        }
        Some(env)
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    fn kind(&self) -> &'static str {
        "telegram"
    }

    fn is_configured(&self) -> bool {
        !self.config.bot_token.is_empty()
    }

    /// Send a single outbound message. Only the configured maintainer slug is
    /// routable in v1 (ADR-056 "Outbound addressing"); any other
    /// `telegram:<slug>` recipient is unroutable and is logged and dropped,
    /// never silently misdelivered to the maintainer chat.
    async fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
        let slug = strip_kind_prefix(&envelope.to, "telegram");
        if slug != self.config.maintainer_slug {
            tracing::warn!(
                slug,
                "telegram: unroutable recipient slug (only the configured maintainer slug is \
                 routable in v1); dropping outbound message"
            );
            return Ok(());
        }
        self.connector
            .send_message(self.config.maintainer_chat_id, &envelope.content)
            .await
    }

    /// Poll `getUpdates` for new inbound messages.
    ///
    /// `since` is unused: Telegram's `getUpdates` offset watermark (held in
    /// memory) is the sole progress mechanism, mirroring the ADR's explicit
    /// choice not to reuse the IMAP-style persisted checkpoint for this
    /// adapter.
    async fn poll(&self, _since: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
        let updates = self.connector.get_updates(self.current_offset()).await?;

        let mut envelopes = Vec::new();
        let mut max_update_id = None;
        for update in &updates {
            max_update_id = Some(
                max_update_id
                    .unwrap_or(update.update_id)
                    .max(update.update_id),
            );
            if let Some(env) = self.envelope_for(update) {
                envelopes.push(env);
            }
        }

        // Advance the offset past every fetched update (authorized or not) so
        // getUpdates never re-delivers an already-seen update, regardless of
        // whether it produced an envelope.
        if let Some(max_id) = max_update_id {
            self.advance_offset(max_id + 1);
        }

        Ok(envelopes)
    }
}

/// Strip a `"kind:"` prefix from an address string.
fn strip_kind_prefix<'a>(addr: &'a str, kind: &str) -> &'a str {
    let prefix = format!("{kind}:");
    addr.strip_prefix(prefix.as_str()).unwrap_or(addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::{TelegramChat, TelegramMessage};
    use std::sync::Mutex as StdMutex;

    fn make_config() -> TelegramChannelConfig {
        TelegramChannelConfig {
            bot_token: "test-token".to_string(), // gitleaks:allow
            maintainer_chat_id: 555,
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

        async fn get_updates(
            &self,
            offset: Option<i64>,
        ) -> Result<Vec<TelegramUpdate>, ChannelError> {
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
                text: Some(text.to_string()),
            }),
        }
    }

    #[test]
    fn kind_is_telegram() {
        let ch =
            TelegramChannel::with_connector(make_config(), Box::new(MockConnector::new(vec![])));
        assert_eq!(ch.kind(), "telegram");
    }

    #[tokio::test]
    async fn poll_authorized_chat_produces_envelope_with_correct_external_id() {
        let updates = vec![vec![text_update(10, 555, "hello khive", 1_700_000_000)]];
        let ch =
            TelegramChannel::with_connector(make_config(), Box::new(MockConnector::new(updates)));

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
        let ch =
            TelegramChannel::with_connector(make_config(), Box::new(MockConnector::new(updates)));

        let envs = ch.poll(Utc::now()).await.unwrap();
        assert!(
            envs.is_empty(),
            "unauthorized chat id must be dropped with no note, not surfaced as an error"
        );
    }

    #[tokio::test]
    async fn poll_bad_message_in_batch_does_not_abort_others() {
        let updates = vec![vec![
            text_update(20, 999, "attacker", 1_700_000_000),
            text_update(21, 555, "maintainer says hi", 1_700_000_001),
        ]];
        let ch =
            TelegramChannel::with_connector(make_config(), Box::new(MockConnector::new(updates)));

        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1, "only the authorized update must be returned");
        assert_eq!(envs[0].external_id.as_deref(), Some("tg:555:21"));
    }

    #[tokio::test]
    async fn poll_advances_offset_past_all_fetched_updates_including_unauthorized() {
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
        let _second = ch.poll(Utc::now()).await.unwrap();

        let seen = connector.offsets_seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![None, Some(3)],
            "second poll must request offset = last update_id + 1"
        );
    }

    #[tokio::test]
    async fn send_to_maintainer_slug_is_routed() {
        let connector = MockConnector::new(vec![]);
        let ch = TelegramChannel::with_connector(make_config(), Box::new(connector));

        let env = ChannelEnvelope::new(BOT_SELF_ADDRESS, "telegram:maintainer", "reply text");
        ch.send(env).await.unwrap();
    }

    #[tokio::test]
    async fn send_to_unroutable_slug_is_dropped_not_misdelivered() {
        let connector = std::sync::Arc::new(MockConnector::new(vec![]));
        let ch = TelegramChannel::with_connector(make_config(), Box::new(connector.clone()));

        let env = ChannelEnvelope::new(BOT_SELF_ADDRESS, "telegram:someone-else", "reply text");
        ch.send(env).await.unwrap();

        assert!(
            connector.sent.lock().unwrap().is_empty(),
            "an unroutable slug must never be sent to the maintainer chat"
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
        let ch =
            TelegramChannel::with_connector(make_config(), Box::new(MockConnector::new(vec![])));
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
        let ch =
            TelegramChannel::with_connector(make_config(), Box::new(MockConnector::new(vec![])));
        assert_eq!(ch.maintainer_slug(), "maintainer");
    }
}
