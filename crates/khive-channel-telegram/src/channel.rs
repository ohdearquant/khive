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
    /// The offset [`Channel::poll`] would advance to, computed from the most
    /// recent batch of fetched updates but not yet applied to `offset`.
    /// Applied only by [`Self::commit_offset`], which callers must invoke
    /// after every authorized envelope from that batch durably ingests.
    /// Until then, the next `poll` call re-requests with the unchanged
    /// `offset`, so a failed batch is re-fetched rather than lost.
    pending_offset: Mutex<Option<i64>>,
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
            pending_offset: Mutex::new(None),
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
                reason = "chat_id_mismatch",
                chat_id = message.chat.id,
                update_id = update.update_id,
                "telegram: update from unauthorized chat id, dropping"
            );
            return None;
        }

        let Some(sender) = message.from.as_ref() else {
            tracing::warn!(
                reason = "missing_sender",
                chat_id = message.chat.id,
                update_id = update.update_id,
                "telegram: update without sender, dropping"
            );
            return None;
        };
        if sender.id != self.config.authorized_sender_id {
            tracing::warn!(
                reason = "sender_id_mismatch",
                chat_id = message.chat.id,
                sender_id = sender.id,
                update_id = update.update_id,
                "telegram: update from unauthorized sender, dropping"
            );
            return None;
        }

        let text = message.text.as_ref()?;
        let external_id = format!("tg:{}:{}", message.chat.id, update.update_id);
        let from = format!("telegram:{}", self.config.maintainer_slug);
        let sent_at = Utc.timestamp_opt(message.date, 0).single();

        let mut env = ChannelEnvelope::new(from.clone(), BOT_SELF_ADDRESS, text.clone())
            .with_external_id(external_id)
            .with_quarantine_replay(text.as_bytes().to_vec(), from);
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
    /// `telegram:<slug>` recipient is rejected as a permanent envelope error,
    /// never silently misdelivered to the maintainer chat.
    async fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
        let slug = strip_kind_prefix(&envelope.to, "telegram");
        if slug != self.config.maintainer_slug {
            tracing::warn!(
                slug,
                "telegram: unroutable recipient slug (only the configured maintainer slug is \
                 routable in v1); rejecting outbound message"
            );
            return Err(ChannelError::InvalidEnvelope(format!(
                "telegram recipient {slug:?} is not routable"
            )));
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
    ///
    /// This does **not** advance the offset used for the next `getUpdates`
    /// call — it only records the candidate advance in `pending_offset`.
    /// Advancing the confirmed offset (so Telegram stops re-delivering this
    /// batch) requires an explicit [`Self::commit_offset`] call, which the
    /// caller must make only after every authorized envelope from this batch
    /// durably ingests. Until committed, the next `poll` reuses the same
    /// offset and re-fetches the same batch.
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

        if let Some(max_id) = max_update_id {
            *self
                .pending_offset
                .lock()
                .expect("pending_offset mutex is never poisoned") = Some(max_id + 1);
        }

        Ok(envelopes)
    }
}

impl TelegramChannel {
    /// Advance the confirmed offset to the value computed by the most
    /// recent [`Channel::poll`] call, so the next `getUpdates` request no
    /// longer re-delivers that batch.
    ///
    /// Callers (the daemon poll loop) must call this only after every
    /// authorized envelope from that batch has durably ingested via
    /// `comm.ingest` — never unconditionally after `poll` returns. A no-op
    /// if there is no pending advance (e.g. an empty batch, or this batch's
    /// advance was already committed).
    pub fn commit_offset(&self) {
        let next = self
            .pending_offset
            .lock()
            .expect("pending_offset mutex is never poisoned")
            .take();
        if let Some(next) = next {
            self.advance_offset(next);
        }
    }
}

/// Strip a `"kind:"` prefix from an address string.
fn strip_kind_prefix<'a>(addr: &'a str, kind: &str) -> &'a str {
    let prefix = format!("{kind}:");
    addr.strip_prefix(prefix.as_str()).unwrap_or(addr)
}

#[cfg(test)]
#[path = "channel/tests.rs"]
mod tests;

#[cfg(feature = "test-support")]
#[path = "test_support.rs"]
pub mod test_support;
