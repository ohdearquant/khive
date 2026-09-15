//! Telegram Bot API channel adapter (ADR-056 Amendment 2026-07-05).
//!
//! Implements the `khive-channel` `Channel` trait over the Telegram Bot API's
//! `sendMessage`/`getUpdates` methods. Plain HTTPS + JSON — no Telegram SDK.

mod channel;
mod config;
mod connector;

pub use channel::TelegramChannel;
pub use config::TelegramChannelConfig;

#[cfg(feature = "test-support")]
pub use channel::test_support;
