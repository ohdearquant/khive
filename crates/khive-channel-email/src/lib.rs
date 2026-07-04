//! Email (SMTP/IMAP) channel adapter for khive (ADR-056).
//!
//! Provides `EmailChannel`, which implements the `Channel` trait from
//! `khive-channel`. Configure exclusively via environment variables; no
//! filesystem config is read.
//!
//! ## Connection variables
//!
//! | Variable | Required | Default | Description |
//! |---|---|---|---|
//! | `KHIVE_EMAIL_SMTP_HOST` | yes | — | SMTP relay host |
//! | `KHIVE_EMAIL_SMTP_PORT` | no | `587` | SMTP port (STARTTLS) |
//! | `KHIVE_EMAIL_IMAP_HOST` | yes | — | IMAP server host |
//! | `KHIVE_EMAIL_IMAP_PORT` | no | `993` | IMAP port (TLS) |
//! | `KHIVE_EMAIL_USERNAME` | yes | — | SMTP/IMAP login username (email address) |
//! | `KHIVE_EMAIL_MAILBOX` | no | same as username | Mailbox used as sender address |
//! | `KHIVE_EMAIL_MAINTAINER_ADDRESS` | yes | — | Single authorized maintainer address |
//! | `KHIVE_EMAIL_AUTHSERV_ID` | yes | — | `authserv-id` this deployment trusts in `Authentication-Results` headers (ADR-056 Amendment 2026-07-02) |
//! | `KHIVE_EMAIL_QUARANTINE_STORE` | no | `true` | Store messages that fail the attribution gate (unattributed) instead of dropping them |
//!
//! ## Auth variables
//!
//! **Basic auth**: set `KHIVE_EMAIL_PASSWORD`.
//!
//! **OAuth (Exchange Online)**: set all three of
//! `KHIVE_EMAIL_OAUTH_TENANT_ID`, `KHIVE_EMAIL_OAUTH_CLIENT_ID`,
//! `KHIVE_EMAIL_OAUTH_CLIENT_SECRET`. A partial set is rejected at startup.
//!
//! ## MCP outbox and routing variables (read by `khive-mcp`)
//!
//! | Variable | Default | Description |
//! |---|---|---|
//! | `KHIVE_EMAIL_DEFAULT_ACTOR` | `lambda:leo` | Actor that receives fresh inbound email (no In-Reply-To match) |
//! | `KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS` | maintainer address | Comma-separated allowlist of recipient addresses the outbox loop may deliver to; defaults to the single maintainer address |
//! | `KHIVE_EMAIL_INGEST_NAMESPACE` | `local` | Namespace used when persisting inbound and outbound messages |

pub(crate) mod auth_results;
pub mod backoff;
pub mod channel;
pub mod config;
pub mod connector;
pub(crate) mod oauth;

pub use backoff::{is_backoff_eligible, BackoffTick, ImapBackoff, ImapSingleFlight};
pub use channel::EmailChannel;
pub use config::EmailChannelConfig;
