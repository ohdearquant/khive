//! Environment-only configuration for the Telegram channel (ADR-056 Amendment
//! 2026-07-05).
//!
//! All settings are read from environment variables. No filesystem config is
//! loaded. Missing required variables produce an error at construction time.

use khive_channel::ChannelError;

/// Default slug for the single routable `telegram:<slug>` recipient (v1 is
/// single-maintainer; see ADR-056 §"Outbound addressing").
const DEFAULT_MAINTAINER_SLUG: &str = "maintainer";

/// Default namespace inbound Telegram messages are ingested into.
const DEFAULT_INGEST_NAMESPACE: &str = "local";

/// Configuration for the Telegram Bot API adapter.
///
/// Build via [`TelegramChannelConfig::from_env`]. All fields are sourced
/// exclusively from environment variables; no defaults are provided for the
/// bot token or the maintainer chat id.
#[derive(Clone)]
pub struct TelegramChannelConfig {
    /// Bot API token (BotFather). Never logged in full.
    pub bot_token: String,
    /// The inbound conversation and outbound recipient for the maintainer slug.
    pub maintainer_chat_id: i64,
    /// The Telegram account authorized to send inbound messages in that chat.
    pub authorized_sender_id: i64,
    /// The slug in `telegram:<slug>` that maps to `maintainer_chat_id`.
    pub maintainer_slug: String,
    /// Target namespace for ingested inbound messages.
    pub ingest_namespace: String,
}

impl std::fmt::Debug for TelegramChannelConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramChannelConfig")
            .field("bot_token", &mask_token(&self.bot_token))
            .field("maintainer_chat_id", &self.maintainer_chat_id)
            .field("authorized_sender_id", &self.authorized_sender_id)
            .field("maintainer_slug", &self.maintainer_slug)
            .field("ingest_namespace", &self.ingest_namespace)
            .finish()
    }
}

/// Mask a secret token for safe logging: `{first6}...[N chars]` (ADR-056 §9).
fn mask_token(token: &str) -> String {
    let visible: String = token.chars().take(6).collect();
    format!("{visible}...[{} chars]", token.chars().count())
}

impl TelegramChannelConfig {
    /// Load configuration from environment variables.
    ///
    /// Required:
    /// - `KHIVE_TELEGRAM_BOT_TOKEN`
    /// - `KHIVE_TELEGRAM_MAINTAINER_CHAT_ID` (numeric)
    /// - `KHIVE_TELEGRAM_AUTHORIZED_SENDER_ID` for non-private chats
    ///
    /// Optional with defaults:
    /// - `KHIVE_TELEGRAM_MAINTAINER_SLUG` (default `"maintainer"`)
    /// - `KHIVE_TELEGRAM_INGEST_NAMESPACE` (default `"local"`)
    pub fn from_env() -> Result<Self, ChannelError> {
        let bot_token = require_nonempty_env("KHIVE_TELEGRAM_BOT_TOKEN")?;

        let chat_id_raw = require_nonempty_env("KHIVE_TELEGRAM_MAINTAINER_CHAT_ID")?;
        let maintainer_chat_id = chat_id_raw.trim().parse::<i64>().map_err(|_| {
            ChannelError::Config(format!(
                "KHIVE_TELEGRAM_MAINTAINER_CHAT_ID must be a valid signed integer, got: {chat_id_raw:?}"
            ))
        })?;
        let authorized_sender_id = match std::env::var("KHIVE_TELEGRAM_AUTHORIZED_SENDER_ID") {
            Ok(raw) if !raw.trim().is_empty() => {
                let sender_id = raw.trim().parse::<i64>().map_err(|_| {
                    ChannelError::Config(
                        "KHIVE_TELEGRAM_AUTHORIZED_SENDER_ID must be a positive integer"
                            .to_string(),
                    )
                })?;
                if sender_id <= 0 {
                    return Err(ChannelError::Config(
                        "KHIVE_TELEGRAM_AUTHORIZED_SENDER_ID must be a positive integer"
                            .to_string(),
                    ));
                }
                sender_id
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(ChannelError::Config(
                    "KHIVE_TELEGRAM_AUTHORIZED_SENDER_ID must be a positive integer".to_string(),
                ));
            }
            Ok(_) | Err(std::env::VarError::NotPresent) if maintainer_chat_id > 0 => {
                maintainer_chat_id
            }
            Ok(_) | Err(std::env::VarError::NotPresent) => {
                return Err(ChannelError::Config(
                    "KHIVE_TELEGRAM_AUTHORIZED_SENDER_ID is required for a non-positive KHIVE_TELEGRAM_MAINTAINER_CHAT_ID"
                        .to_string(),
                ));
            }
        };

        let maintainer_slug = match std::env::var("KHIVE_TELEGRAM_MAINTAINER_SLUG") {
            Ok(v) if !v.trim().is_empty() => v,
            _ => DEFAULT_MAINTAINER_SLUG.to_string(),
        };

        let ingest_namespace = match std::env::var("KHIVE_TELEGRAM_INGEST_NAMESPACE") {
            Ok(v) if !v.trim().is_empty() => v,
            _ => DEFAULT_INGEST_NAMESPACE.to_string(),
        };

        Ok(Self {
            bot_token,
            maintainer_chat_id,
            authorized_sender_id,
            maintainer_slug,
            ingest_namespace,
        })
    }
}

fn require_nonempty_env(key: &str) -> Result<String, ChannelError> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Ok(v),
        Ok(_) => Err(ChannelError::Config(format!(
            "environment variable {key:?} must not be empty"
        ))),
        Err(_) => Err(ChannelError::Config(format!(
            "required environment variable {key:?} is not set"
        ))),
    }
}

#[cfg(test)]
#[path = "config/tests.rs"]
mod tests;
