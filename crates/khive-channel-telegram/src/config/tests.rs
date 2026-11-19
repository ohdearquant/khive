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
    "KHIVE_TELEGRAM_AUTHORIZED_SENDER_ID",
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
    std::env::set_var("KHIVE_TELEGRAM_MAINTAINER_CHAT_ID", "98765");
    std::env::remove_var("KHIVE_TELEGRAM_AUTHORIZED_SENDER_ID");
    std::env::remove_var("KHIVE_TELEGRAM_MAINTAINER_SLUG");
    std::env::remove_var("KHIVE_TELEGRAM_INGEST_NAMESPACE");

    let config = TelegramChannelConfig::from_env().expect("valid config must succeed");
    assert_eq!(config.maintainer_chat_id, 98765);
    assert_eq!(config.authorized_sender_id, 98765);
    assert_eq!(config.maintainer_slug, "maintainer");
    assert_eq!(config.ingest_namespace, "local");
}

#[test]
fn from_env_respects_overridden_slug_and_namespace() {
    let _guard = ENV_MUTEX.lock().unwrap();
    let _snap = EnvSnapshot::capture(KEYS);
    std::env::set_var("KHIVE_TELEGRAM_BOT_TOKEN", "test-token"); // gitleaks:allow
    std::env::set_var("KHIVE_TELEGRAM_MAINTAINER_CHAT_ID", "42");
    std::env::remove_var("KHIVE_TELEGRAM_AUTHORIZED_SENDER_ID");
    std::env::set_var("KHIVE_TELEGRAM_MAINTAINER_SLUG", "alice");
    std::env::set_var("KHIVE_TELEGRAM_INGEST_NAMESPACE", "lambda:alice");

    let config = TelegramChannelConfig::from_env().expect("valid config must succeed");
    assert_eq!(config.maintainer_slug, "alice");
    assert_eq!(config.ingest_namespace, "lambda:alice");
}

#[test]
fn group_chat_requires_explicit_authorized_sender_id() {
    let _guard = ENV_MUTEX.lock().unwrap();
    let _snap = EnvSnapshot::capture(KEYS);
    std::env::set_var("KHIVE_TELEGRAM_BOT_TOKEN", "test-token"); // gitleaks:allow
    std::env::set_var("KHIVE_TELEGRAM_MAINTAINER_CHAT_ID", "-100123");
    std::env::remove_var("KHIVE_TELEGRAM_AUTHORIZED_SENDER_ID");

    let err = TelegramChannelConfig::from_env().unwrap_err();
    assert!(err
        .to_string()
        .contains("KHIVE_TELEGRAM_AUTHORIZED_SENDER_ID"));
    std::env::set_var("KHIVE_TELEGRAM_AUTHORIZED_SENDER_ID", "42");
    let config = TelegramChannelConfig::from_env().unwrap();
    assert_eq!(config.maintainer_chat_id, -100123);
    assert_eq!(config.authorized_sender_id, 42);
}

#[test]
fn invalid_explicit_sender_id_never_falls_back_to_chat_id() {
    let _guard = ENV_MUTEX.lock().unwrap();
    let _snap = EnvSnapshot::capture(KEYS);
    std::env::set_var("KHIVE_TELEGRAM_BOT_TOKEN", "test-token"); // gitleaks:allow
    std::env::set_var("KHIVE_TELEGRAM_MAINTAINER_CHAT_ID", "42");
    for invalid in ["not-a-number", "0", "-1"] {
        std::env::set_var("KHIVE_TELEGRAM_AUTHORIZED_SENDER_ID", invalid);
        let err = TelegramChannelConfig::from_env().unwrap_err();
        assert!(err
            .to_string()
            .contains("KHIVE_TELEGRAM_AUTHORIZED_SENDER_ID"));
    }
}

#[test]
fn debug_output_masks_bot_token() {
    let config = TelegramChannelConfig {
        bot_token: "123456:AAFakeTokenValueForTestingOnly".to_string(), // gitleaks:allow
        maintainer_chat_id: 1,
        authorized_sender_id: 1,
        maintainer_slug: "maintainer".to_string(),
        ingest_namespace: "local".to_string(),
    };
    let debug_output = format!("{config:?}");
    assert!(!debug_output.contains("AAFakeTokenValueForTestingOnly"));
    assert!(debug_output.contains("123456"));
    assert!(debug_output.contains("chars]"));
}
