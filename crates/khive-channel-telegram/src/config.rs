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
    /// The single authorized inbound sender AND the outbound recipient for
    /// the maintainer slug.
    pub maintainer_chat_id: i64,
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
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Environment variables are process-global; serialize env-mutating tests
    // and restore prior values on drop (mirrors khive-channel-email's pattern).
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    struct EnvSnapshot {
        vars: Vec<(String, Option<String>)>,
    }

    impl EnvSnapshot {
        fn capture(keys: &[&str]) -> Self {
            Self {
                vars: keys
                    .iter()
                    .map(|k| (k.to_string(), std::env::var(k).ok()))
                    .collect(),
            }
        }
    }

    impl Drop for EnvSnapshot {
        fn drop(&mut self) {
            for (k, v) in &self.vars {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    const KEYS: &[&str] = &[
        "KHIVE_TELEGRAM_BOT_TOKEN",
        "KHIVE_TELEGRAM_MAINTAINER_CHAT_ID",
        "KHIVE_TELEGRAM_MAINTAINER_SLUG",
        "KHIVE_TELEGRAM_INGEST_NAMESPACE",
    ];

    #[test]
    fn from_env_missing_bot_token_fails_closed() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _snap = EnvSnapshot::capture(KEYS);
        std::env::remove_var("KHIVE_TELEGRAM_BOT_TOKEN");
        std::env::set_var("KHIVE_TELEGRAM_MAINTAINER_CHAT_ID", "12345");

        let err = TelegramChannelConfig::from_env().unwrap_err();
        assert!(err.to_string().contains("KHIVE_TELEGRAM_BOT_TOKEN"));
    }

    #[test]
    fn from_env_missing_chat_id_fails_closed() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _snap = EnvSnapshot::capture(KEYS);
        std::env::set_var("KHIVE_TELEGRAM_BOT_TOKEN", "test-token"); // gitleaks:allow
        std::env::remove_var("KHIVE_TELEGRAM_MAINTAINER_CHAT_ID");

        let err = TelegramChannelConfig::from_env().unwrap_err();
        assert!(err
            .to_string()
            .contains("KHIVE_TELEGRAM_MAINTAINER_CHAT_ID"));
    }

    #[test]
    fn from_env_non_numeric_chat_id_fails_closed() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _snap = EnvSnapshot::capture(KEYS);
        std::env::set_var("KHIVE_TELEGRAM_BOT_TOKEN", "test-token"); // gitleaks:allow
        std::env::set_var("KHIVE_TELEGRAM_MAINTAINER_CHAT_ID", "not-a-number");

        let err = TelegramChannelConfig::from_env().unwrap_err();
        assert!(err
            .to_string()
            .contains("KHIVE_TELEGRAM_MAINTAINER_CHAT_ID"));
    }

    #[test]
    fn from_env_defaults_slug_and_namespace() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _snap = EnvSnapshot::capture(KEYS);
        std::env::set_var("KHIVE_TELEGRAM_BOT_TOKEN", "test-token"); // gitleaks:allow
        std::env::set_var("KHIVE_TELEGRAM_MAINTAINER_CHAT_ID", "-98765");
        std::env::remove_var("KHIVE_TELEGRAM_MAINTAINER_SLUG");
        std::env::remove_var("KHIVE_TELEGRAM_INGEST_NAMESPACE");

        let config = TelegramChannelConfig::from_env().expect("valid config must succeed");
        assert_eq!(config.maintainer_chat_id, -98765);
        assert_eq!(config.maintainer_slug, "maintainer");
        assert_eq!(config.ingest_namespace, "local");
    }

    #[test]
    fn from_env_respects_overridden_slug_and_namespace() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _snap = EnvSnapshot::capture(KEYS);
        std::env::set_var("KHIVE_TELEGRAM_BOT_TOKEN", "test-token"); // gitleaks:allow
        std::env::set_var("KHIVE_TELEGRAM_MAINTAINER_CHAT_ID", "42");
        std::env::set_var("KHIVE_TELEGRAM_MAINTAINER_SLUG", "leo");
        std::env::set_var("KHIVE_TELEGRAM_INGEST_NAMESPACE", "lambda:leo");

        let config = TelegramChannelConfig::from_env().expect("valid config must succeed");
        assert_eq!(config.maintainer_slug, "leo");
        assert_eq!(config.ingest_namespace, "lambda:leo");
    }

    #[test]
    fn debug_output_masks_bot_token() {
        let config = TelegramChannelConfig {
            bot_token: "123456:AAFakeTokenValueForTestingOnly".to_string(), // gitleaks:allow
            maintainer_chat_id: 1,
            maintainer_slug: "maintainer".to_string(),
            ingest_namespace: "local".to_string(),
        };
        let debug_output = format!("{config:?}");
        assert!(!debug_output.contains("AAFakeTokenValueForTestingOnly"));
        assert!(debug_output.contains("123456"));
        assert!(debug_output.contains("chars]"));
    }
}
