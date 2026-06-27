//! Email (SMTP/IMAP) channel adapter for khive (ADR-056).
//!
//! Provides `EmailChannel`, which implements the `Channel` trait from
//! `khive-channel`. Configure exclusively via environment variables; no
//! filesystem config is read.

pub mod channel;
pub mod config;
pub mod connector;

pub use channel::EmailChannel;
pub use config::EmailChannelConfig;
