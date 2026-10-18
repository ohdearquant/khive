//! `EmailChannel` — implements the `Channel` trait for email transport.

use std::num::NonZeroU32;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use khive_channel::{
    Channel, ChannelCheckpoint, ChannelEnvelope, ChannelError, ChannelPollPage,
    InboundChannelDelivery, StoredChannelCheckpoint,
};
use mail_parser::MessageParser;
use tracing::{debug, warn};

use crate::auth_results;
use crate::config::{EmailAuth, EmailChannelConfig, ExternalInboundConfig};
use crate::connector::imap::{ImapFetcher, ImapProgress, MalformedReason, SelectedMessage};
use crate::connector::smtp::SmtpSender;
use crate::connector::{MailAddress, RawEmail};
use crate::external_host::{ExternalEmailHost, InboundSuppression};
use crate::external_inbound::{
    external_delivery_marking, owner_candidates, route_inbound, InboundRoute,
};
use crate::oauth::TokenProvider;
use crate::screening::{screen_message, Disposition, ScreeningOutcome};
use crate::unsubscribe::extract_unsubscribe_token;

mod channel_impl;
mod email_channel;

use channel_impl::*;

/// Literal IMAP folder this connector selects. Not the configured mailbox
/// address (which identifies the account/credential, not the folder).
const IMAP_FOLDER: &str = "INBOX";

/// Page size for both the legacy `poll` path and the checkpointed
/// `poll_page` path.
const IMAP_PAGE_LIMIT: usize = 50;

/// Scope the connector's pre-account UID key to the configured mailbox.
/// Keep the connector's original key separately for the read-only migration
/// lookup; new rows are written only under this account-scoped form.
fn account_scoped_imap_id(old_id: &str, mailbox: &str) -> String {
    let account = mailbox.to_lowercase();
    if let Some((host_and_validity, uid)) = old_id.rsplit_once(':') {
        if let Some((host, validity)) = host_and_validity.rsplit_once(':') {
            if host.starts_with("imap:") {
                return format!("{host}:{account}:{validity}:{uid}");
            }
        }
    }
    // An alternate connector can supply an opaque key. Still include the
    // account so an unexpected format cannot collapse two credentials.
    format!("imap:{account}:{old_id}")
}

// Quarantine sender prefix invariant: `email:quarantine` must retain the
// channel's `email:` prefix because prefix-keyed consumers use it to surface
// the notification. Renaming it outside that prefix silently hides alerts.
const EMAIL_QUARANTINE_SENDER: &str = "email:quarantine";

/// Reason a message failed the attribution gate (ADR-056 Amendment
/// 2026-07-02) and was quarantined instead of attributed to the maintainer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuarantineReason {
    /// Neither plain nor ARC authentication-results headers were present.
    AuthAbsent,
    /// Authentication-results headers were present, but none was trusted.
    AuthUntrusted,
    /// A trusted header was selected but showed no passing, aligned method.
    DmarcFail,
    /// spf or dkim passed, but its alignment domain did not match the From: domain.
    Unaligned,
    /// Domain authentication passed, but the sender is not on the maintainer allowlist.
    OffAllowlist,
    /// The sender authenticated, but an `inbound` suppression entry matches the `From:` address or
    /// its exact domain (ADR-200 §7 step 6).
    Blocked,
    /// Deterministic screening requires quarantine (ADR-200 section 8.3).
    Screened,
    /// The `UID FETCH` response carried no RFC822 body for this UID (khive
    /// #449 High fix: a durable terminal disposition, never a silent drop).
    MissingBody,
    /// An RFC822 body was present but could not be parsed (khive #449 High
    /// fix: a durable terminal disposition, never a silent drop).
    ParseFailure,
    /// The message exceeds the configured inbound byte limit.
    TooLarge,
}

impl std::fmt::Display for QuarantineReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            QuarantineReason::AuthAbsent => "auth-absent",
            QuarantineReason::AuthUntrusted => "auth-untrusted",
            QuarantineReason::DmarcFail => "dmarc-fail",
            QuarantineReason::Unaligned => "unaligned",
            QuarantineReason::OffAllowlist => "off-allowlist",
            QuarantineReason::Blocked => "blocked",
            QuarantineReason::Screened => "screened",
            QuarantineReason::MissingBody => "missing-body",
            QuarantineReason::ParseFailure => "parse-failure",
            QuarantineReason::TooLarge => "too-large",
        })
    }
}

impl From<MalformedReason> for QuarantineReason {
    fn from(reason: MalformedReason) -> Self {
        match reason {
            MalformedReason::MissingBody => QuarantineReason::MissingBody,
            MalformedReason::ParseFailure => QuarantineReason::ParseFailure,
            MalformedReason::TooLarge => QuarantineReason::TooLarge,
        }
    }
}

/// A mailto unsubscribe (ADR-200 §5.4 entry point 1) whose token verified during the synchronous
/// disposition loop, paired with the IMAP UID it came from so a failed store write can be
/// retried without re-parsing and without logging the sender's address.
struct PendingUnsubscribe {
    uid: u32,
    recipient_key: String,
}

enum EmailAttribution {
    Maintainer,
    External,
}

/// Email channel adapter implementing `Channel` via SMTP + IMAP.
///
/// Configuration is provided at construction time via `EmailChannelConfig`.
/// All credentials come from environment variables; see `EmailChannelConfig::from_env`.
pub struct EmailChannel {
    config: EmailChannelConfig,
    smtp: SmtpSender,
    imap: ImapFetcher,
    /// The host-supplied external-email services: the unsubscribe token verifier, the inbound
    /// suppression read, and the mailto opt-out write (ADR-200 §5.1/§5.4/§7). Unset for the many
    /// tests that build a channel directly; with it unset no token can verify and no inbound list
    /// is read, matching the `KHIVE_EMAIL_EXTERNAL_SENDERS`-unset production behaviour.
    external_host: Option<Arc<dyn ExternalEmailHost>>,
    external_inbound: Option<ExternalInboundConfig>,
}

#[cfg(test)]
mod tests;
