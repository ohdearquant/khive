//! `EmailChannel` — implements the `Channel` trait for email transport.

use std::num::NonZeroU32;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use khive_channel::{
    Channel, ChannelCheckpoint, ChannelEnvelope, ChannelError, ChannelPollPage,
    StoredChannelCheckpoint,
};
use tracing::{debug, warn};

use crate::auth_results;
use crate::config::{EmailAuth, EmailChannelConfig};
use crate::connector::imap::{ImapFetcher, ImapProgress, MalformedReason, SelectedMessage};
use crate::connector::smtp::SmtpSender;
use crate::connector::{MailAddress, RawEmail};
use crate::oauth::TokenProvider;

/// Literal IMAP folder this connector selects. Not the configured mailbox
/// address (which identifies the account/credential, not the folder).
const IMAP_FOLDER: &str = "INBOX";

/// Page size for both the legacy `poll` path and the checkpointed
/// `poll_page` path.
const IMAP_PAGE_LIMIT: usize = 50;

/// Reason a message failed the attribution gate (ADR-056 Amendment
/// 2026-07-02) and was quarantined instead of attributed to the maintainer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuarantineReason {
    /// No `Authentication-Results` header was selected: none were present, or
    /// none carried the configured `authserv-id`.
    AuthAbsent,
    /// A trusted header was selected but showed no passing, aligned method.
    DmarcFail,
    /// spf or dkim passed, but its alignment domain did not match the From: domain.
    Unaligned,
    /// Domain authentication passed, but the sender is not on the maintainer allowlist.
    OffAllowlist,
    /// The `UID FETCH` response carried no RFC822 body for this UID (khive
    /// #449 High fix: a durable terminal disposition, never a silent drop).
    MissingBody,
    /// An RFC822 body was present but could not be parsed (khive #449 High
    /// fix: a durable terminal disposition, never a silent drop).
    ParseFailure,
}

impl std::fmt::Display for QuarantineReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            QuarantineReason::AuthAbsent => "auth-absent",
            QuarantineReason::DmarcFail => "dmarc-fail",
            QuarantineReason::Unaligned => "unaligned",
            QuarantineReason::OffAllowlist => "off-allowlist",
            QuarantineReason::MissingBody => "missing-body",
            QuarantineReason::ParseFailure => "parse-failure",
        })
    }
}

impl From<MalformedReason> for QuarantineReason {
    fn from(reason: MalformedReason) -> Self {
        match reason {
            MalformedReason::MissingBody => QuarantineReason::MissingBody,
            MalformedReason::ParseFailure => QuarantineReason::ParseFailure,
        }
    }
}

/// Email channel adapter implementing `Channel` via SMTP + IMAP.
///
/// Configuration is provided at construction time via `EmailChannelConfig`.
/// All credentials come from environment variables; see `EmailChannelConfig::from_env`.
pub struct EmailChannel {
    config: EmailChannelConfig,
    smtp: SmtpSender,
    imap: ImapFetcher,
}

impl EmailChannel {
    /// Build from environment variables. Fails if any required env var is absent.
    pub fn from_env() -> Result<Self, ChannelError> {
        let config = EmailChannelConfig::from_env()?;

        let (smtp, imap) = match &config.auth {
            EmailAuth::Basic { password } => {
                let smtp = SmtpSender::new(
                    &config.smtp_host,
                    config.smtp_port,
                    &config.username,
                    password,
                );
                let imap = ImapFetcher::new(
                    &config.imap_host,
                    config.imap_port,
                    &config.username,
                    password,
                );
                (smtp, imap)
            }
            EmailAuth::OAuth {
                tenant_id,
                client_id,
                client_secret,
            } => {
                let token_provider = Arc::new(TokenProvider::new(
                    tenant_id.clone(),
                    client_id.clone(),
                    client_secret.clone(),
                ));
                let smtp = SmtpSender::new_oauth(
                    &config.smtp_host,
                    config.smtp_port,
                    &config.mailbox,
                    Arc::clone(&token_provider),
                );
                let imap = ImapFetcher::new_oauth(
                    &config.imap_host,
                    config.imap_port,
                    &config.mailbox,
                    Arc::clone(&token_provider),
                );
                (smtp, imap)
            }
        };

        Ok(Self { config, smtp, imap })
    }

    /// The configured mailbox address (e.g. `mailbox@example.com`).
    ///
    /// Used by the outbox delivery loop to derive the sender address and
    /// the domain component of the RFC 822 Message-ID header.
    pub fn mailbox(&self) -> &str {
        &self.config.mailbox
    }

    /// The configured maintainer address as a string.
    ///
    /// Used by the outbox delivery loop to build the default allowlist when
    /// `KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS` is not set.
    pub fn maintainer_address(&self) -> &str {
        // Primary maintainer: from_env guarantees `maintainer_addresses` is non-empty.
        self.config.maintainer_addresses[0].as_str()
    }

    /// Build from pre-constructed connectors (for testing).
    #[cfg(test)]
    pub(crate) fn with_connectors(
        config: EmailChannelConfig,
        smtp: SmtpSender,
        imap: ImapFetcher,
    ) -> Self {
        Self { config, smtp, imap }
    }

    /// Validate that the message sender is authorized.
    ///
    /// Rules:
    /// - `from_addrs` must contain exactly one entry (multi-From is rejected).
    /// - That single From address must match one of the authorized maintainers
    ///   (Gmail-aware: dots/`+tag`/`googlemail.com` are insignificant for Gmail).
    /// - If `sender_addr` is present, it must also match an authorized maintainer.
    ///
    /// Returns `Err(ChannelError::UnauthorizedSender)` on any violation.
    /// Error messages intentionally omit the actual addresses to avoid leaking them to logs.
    fn check_sender(
        &self,
        from_addrs: &[String],
        sender_addr: Option<&str>,
    ) -> Result<(), ChannelError> {
        // Exactly one From address is required. Zero or more-than-one is an unauthorized state.
        if from_addrs.len() != 1 {
            return Err(ChannelError::UnauthorizedSender(format!(
                "expected exactly 1 From address, got {}",
                from_addrs.len()
            )));
        }
        let from = MailAddress::parse(&from_addrs[0]).ok_or_else(|| {
            ChannelError::UnauthorizedSender("From field does not contain a valid addr-spec".into())
        })?;
        if !self
            .config
            .maintainer_addresses
            .iter()
            .any(|m| m.matches(&from))
        {
            return Err(ChannelError::UnauthorizedSender(
                "From address is not an authorized maintainer".into(),
            ));
        }
        // Sender header, when present, must also match an authorized maintainer.
        if let Some(s) = sender_addr {
            let sender = MailAddress::parse(s).ok_or_else(|| {
                ChannelError::UnauthorizedSender(
                    "Sender field does not contain a valid addr-spec".into(),
                )
            })?;
            if !self
                .config
                .maintainer_addresses
                .iter()
                .any(|m| m.matches(&sender))
            {
                return Err(ChannelError::UnauthorizedSender(
                    "Sender address is not an authorized maintainer".into(),
                ));
            }
        }
        Ok(())
    }

    /// Evaluate the domain-authentication half of the attribution gate
    /// (ADR-056 Amendment 2026-07-02, refined 2026-07-03): trust only the
    /// header selected per this deployment's configured [`TrustAnchor`]
    /// (topmost matching `authserv-id`, or the topmost no-authserv-id header
    /// for an EXO-shaped boundary), and require `dmarc=pass` aligned to the
    /// From domain, or `spf=pass` with an aligned envelope-from, or
    /// `dkim=pass` with an aligned `d=` domain. Absence of any trusted header
    /// fails closed.
    fn evaluate_auth(&self, email: &RawEmail) -> Result<(), QuarantineReason> {
        let selected =
            auth_results::select_trusted(&email.authentication_results, &self.config.trust_anchor)
                .ok_or(QuarantineReason::AuthAbsent)?;

        let from_domain = email
            .from_addrs
            .first()
            .and_then(|addr| addr.rsplit_once('@'))
            .map(|(_, domain)| domain.to_lowercase())
            .unwrap_or_default();

        if selected.dmarc_pass_aligned(&from_domain)
            || selected.spf_pass_aligned(&from_domain)
            || selected.dkim_pass_aligned(&from_domain)
        {
            return Ok(());
        }
        if selected.has_unaligned_pass(&from_domain) {
            return Err(QuarantineReason::Unaligned);
        }
        Err(QuarantineReason::DmarcFail)
    }

    /// Run the full attribution gate: domain authentication, then the sender
    /// allowlist. Both must pass for a message to be attributed to the
    /// maintainer; either failing quarantines it (never partially attributed).
    fn gate(&self, email: &RawEmail) -> Result<(), QuarantineReason> {
        self.evaluate_auth(email)?;
        self.check_sender(&email.from_addrs, email.sender_addr.as_deref())
            .map_err(|_| QuarantineReason::OffAllowlist)
    }

    /// Build the envelope recorded for a message that failed the attribution
    /// gate. Per ADR-056 Amendment 2026-07-02, a quarantined message must
    /// never be attributed to (or look like) the maintainer: `from` is the
    /// fixed `email:quarantine` marker rather than the message's own
    /// (possibly forged) From address, and no correlation key is set --  an
    /// unauthenticated message claiming to be a reply must not inherit a
    /// legitimate thread's context.
    fn quarantine_envelope(&self, email: &RawEmail, reason: QuarantineReason) -> ChannelEnvelope {
        let to = format!("email:{}", self.maintainer_address());
        let mut env = ChannelEnvelope::new("email:quarantine", to, email.best_body());

        if !email.subject.is_empty() {
            env = env.with_subject(&email.subject);
        }
        if let Some(date) = email.date {
            env = env.with_sent_at(date);
        }
        env = env.with_external_id(&email.imap_external_id);

        env.metadata
            .insert("quarantined".to_string(), "true".to_string());
        env.metadata
            .insert("quarantine_reason".to_string(), reason.to_string());
        if let Some(claimed_from) = email.from_addrs.first() {
            env.metadata
                .insert("quarantine_claimed_from".to_string(), claimed_from.clone());
        }

        env
    }

    /// Build the envelope recorded for a selected UID that could not be
    /// durably parsed into a message (khive #449 High fix: a missing RFC822
    /// body or an unparseable one). Unlike [`Self::quarantine_envelope`],
    /// this is never gated by `quarantine_store` -- a data-integrity failure
    /// must always leave a queryable record, never a silent drop, since
    /// dropping it here is the only way this UID's disposition could be lost
    /// before the cursor advances past it.
    fn malformed_quarantine_envelope(
        &self,
        uid: u32,
        imap_external_id: &str,
        reason: QuarantineReason,
    ) -> ChannelEnvelope {
        let to = format!("email:{}", self.maintainer_address());
        let body =
            format!("(khive: IMAP message UID {uid} could not be parsed and was quarantined)");
        let mut env = ChannelEnvelope::new("email:quarantine", to, body);
        env = env.with_external_id(imap_external_id);
        env.metadata
            .insert("quarantined".to_string(), "true".to_string());
        env.metadata
            .insert("quarantine_reason".to_string(), reason.to_string());
        env
    }

    /// This channel's stable, non-secret checkpoint identity.
    ///
    /// Compared verbatim (never parsed) against a stored checkpoint's
    /// `source` to detect a host/port/mailbox/folder configuration change —
    /// in which case the stored generation/high-water must not be applied to
    /// the current (different) source.
    fn checkpoint_source(&self) -> String {
        format!(
            "imap+tls:{}:{}:{}:{}",
            self.config.imap_host.to_lowercase(),
            self.config.imap_port,
            self.config.mailbox,
            IMAP_FOLDER
        )
    }

    /// Decode a stored checkpoint into the connector-level [`ImapProgress`],
    /// plus the `committed_at` recovery floor (only when the source matches).
    ///
    /// A source mismatch or absent checkpoint yields default (empty)
    /// progress and no recovery floor — a different account's high-water or
    /// timestamp must never apply to this one. A persisted value that does
    /// not fit a nonzero `u32` fails closed with `ChannelError::Config`
    /// rather than silently resetting.
    fn decode_progress(
        &self,
        checkpoint: Option<&StoredChannelCheckpoint>,
    ) -> Result<(ImapProgress, Option<DateTime<Utc>>), ChannelError> {
        let Some(stored) = checkpoint else {
            return Ok((ImapProgress::default(), None));
        };
        if stored.checkpoint.source != self.checkpoint_source() {
            return Ok((ImapProgress::default(), None));
        }

        let uid_validity = u32::try_from(stored.checkpoint.generation)
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or_else(|| {
                ChannelError::Config(format!(
                    "persisted IMAP checkpoint generation {} is not a valid nonzero u32",
                    stored.checkpoint.generation
                ))
            })?;
        let last_seen_uid =
            match stored.checkpoint.high_water {
                None => None,
                Some(h) => Some(u32::try_from(h).ok().and_then(NonZeroU32::new).ok_or_else(
                    || {
                        ChannelError::Config(format!(
                            "persisted IMAP checkpoint high_water {h} is not a valid nonzero u32"
                        ))
                    },
                )?),
            };

        Ok((
            ImapProgress {
                uid_validity: Some(uid_validity),
                last_seen_uid,
            },
            Some(stored.committed_at),
        ))
    }

    /// Encode connector-level progress into the transport-neutral checkpoint
    /// the poll coordinator persists.
    fn encode_checkpoint(&self, progress: ImapProgress) -> ChannelCheckpoint {
        ChannelCheckpoint {
            source: self.checkpoint_source(),
            generation: progress
                .uid_validity
                .map(|v| u64::from(v.get()))
                .unwrap_or(0),
            high_water: progress.last_seen_uid.map(|v| u64::from(v.get())),
        }
    }

    /// Run the gate/quarantine disposition loop over a fetched batch,
    /// producing the envelopes ready for `comm.ingest`. Extracted from `poll`
    /// so `poll_page` shares the exact same per-message logic.
    ///
    /// Every [`SelectedMessage`] produces exactly one disposition here (khive
    /// #449 High fix): a parsed email is gated as before, and a
    /// [`SelectedMessage::Malformed`] entry always becomes a quarantine
    /// envelope, regardless of `quarantine_store` -- see
    /// [`Self::malformed_quarantine_envelope`].
    fn disposition(&self, raw: Vec<SelectedMessage>) -> Vec<ChannelEnvelope> {
        let mut envelopes = Vec::new();
        for message in raw {
            match message {
                SelectedMessage::Email(email) => {
                    let uid = email.uid;
                    match self.gate(&email) {
                        Ok(()) => envelopes.push(self.to_envelope(*email)),
                        Err(reason) => {
                            if self.config.quarantine_store {
                                envelopes.push(self.quarantine_envelope(&email, reason));
                            } else {
                                // Address-free: only the IMAP UID and the quarantine reason
                                // are logged, never the (possibly forged) From address.
                                warn!(
                                    uid,
                                    reason = %reason,
                                    "quarantine-store disabled: dropping unattributed message"
                                );
                            }
                        }
                    }
                }
                SelectedMessage::Malformed {
                    uid,
                    imap_external_id,
                    reason,
                } => {
                    let reason: QuarantineReason = reason.into();
                    warn!(
                        uid,
                        reason = %reason,
                        "quarantining permanently unparseable IMAP message"
                    );
                    envelopes.push(self.malformed_quarantine_envelope(
                        uid,
                        &imap_external_id,
                        reason,
                    ));
                }
            }
        }
        envelopes
    }

    /// Convert a `RawEmail` that has already passed `gate` into a `ChannelEnvelope`.
    fn to_envelope(&self, email: RawEmail) -> ChannelEnvelope {
        // Safe: gate() verified exactly one From entry before this is called.
        let from_addr = &email.from_addrs[0];
        let from = format!("email:{from_addr}");
        let to = email
            .to
            .first()
            .map(|a| format!("email:{a}"))
            .unwrap_or_else(|| format!("email:{}", self.maintainer_address()));

        let mut env = ChannelEnvelope::new(from, to, email.best_body());

        if !email.subject.is_empty() {
            env = env.with_subject(&email.subject);
        }
        if let Some(date) = email.date {
            env = env.with_sent_at(date);
        }
        // Always set external_id from the stable IMAP-based dedup key. Never derive it
        // from Message-ID, which is optional and could be absent or absent-by-design.
        env = env.with_external_id(&email.imap_external_id);
        if let Some(corr) = email.correlation() {
            env = env.with_correlation(corr);
        }
        // Capture this email's own Message-ID so a future reply to it can set
        // In-Reply-To/References for native MUA threading (distinct from external_id).
        if let Some(mid) = email.message_id() {
            env = env.with_wire_message_id(mid);
        }
        // Capture this email's own References chain so a future reply can extend
        // the full ancestor chain instead of truncating it to the immediate parent.
        if let Some(refs) = email.references() {
            env = env.with_wire_references(refs);
        }

        env
    }
}

#[async_trait]
impl Channel for EmailChannel {
    fn kind(&self) -> &'static str {
        "email"
    }

    /// khive #606: the mailbox address distinguishes multiple configured
    /// email accounts (all `kind() == "email"`) so their channel health
    /// rows never collapse into one.
    fn slug(&self) -> String {
        self.config.mailbox.clone()
    }

    async fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
        let from = strip_kind_prefix(&envelope.from, "email");
        let to = strip_kind_prefix(&envelope.to, "email");
        let subject = envelope.subject.as_deref().unwrap_or("(no subject)");
        let thread_id = envelope.correlation_external_id.as_deref();
        let message_id = envelope.message_id.as_deref();
        let in_reply_to = envelope.in_reply_to.as_deref();
        let references = envelope.references.as_deref();

        debug!(from, to, subject, "email send");
        self.smtp
            .send(
                from,
                to,
                subject,
                &envelope.content,
                thread_id,
                message_id,
                in_reply_to,
                references,
            )
            .await
    }

    async fn poll(&self, since: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
        let raw = self.imap.fetch_since(since, IMAP_PAGE_LIMIT).await?;
        Ok(self.disposition(raw))
    }

    /// Checkpointed poll (issue #449): resolves the durable IMAP
    /// UIDVALIDITY/high-water progress from `checkpoint`, fetches strictly
    /// above it (or bootstraps by date on no/mismatched/reset progress), and
    /// returns a checkpoint candidate for the poll coordinator to persist
    /// only after every envelope is durably ingested.
    async fn poll_page(
        &self,
        since: DateTime<Utc>,
        checkpoint: Option<&StoredChannelCheckpoint>,
    ) -> Result<ChannelPollPage, ChannelError> {
        let (progress, committed_at) = self.decode_progress(checkpoint)?;
        let recovery_since = match committed_at {
            Some(committed_at) => since.min(committed_at),
            None => since,
        };

        let page = self
            .imap
            .fetch_page(recovery_since, IMAP_PAGE_LIMIT, progress)
            .await?;
        let envelopes = self.disposition(page.emails);
        let next_checkpoint = if page.next_progress == progress {
            None
        } else {
            Some(self.encode_checkpoint(page.next_progress))
        };

        Ok(ChannelPollPage {
            envelopes,
            next_checkpoint,
        })
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
    use crate::config::{EmailAuth, TrustAnchor};
    use crate::connector::imap::{
        parse_raw_bytes, ImapConnector, ImapFetchPage, ImapFetcher, ImapProgress,
    };
    use crate::connector::smtp::{SmtpConnector, SmtpSender};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// The `authserv-id` this test suite's channels are configured to trust.
    const TEST_AUTHSERV_ID: &str = "mx.example.com";

    fn make_config(maintainer: &str) -> EmailChannelConfig {
        EmailChannelConfig {
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            imap_host: "imap.example.com".to_string(),
            imap_port: 993,
            username: "user@example.com".to_string(),
            mailbox: "user@example.com".to_string(),
            auth: EmailAuth::Basic {
                password: "secret".to_string(),
            },
            maintainer_addresses: vec![MailAddress::parse(maintainer).unwrap()],
            trust_anchor: TrustAnchor::AuthservId(TEST_AUTHSERV_ID.to_string()),
            quarantine_store: true,
        }
    }

    /// A trusted `Authentication-Results` header that authenticates cleanly
    /// for `from_addr`'s own domain, so tests that are exercising allowlist
    /// logic (not the auth gate) get an attributed baseline by default.
    fn trusted_auth_header(from_addr: &str) -> String {
        let domain = from_addr
            .rsplit_once('@')
            .map(|(_, d)| d)
            .unwrap_or("example.com");
        format!("{TEST_AUTHSERV_ID}; dmarc=pass header.from={domain}")
    }

    struct RecordingSmtp {
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl SmtpConnector for RecordingSmtp {
        async fn deliver(
            &self,
            from: &str,
            to: &str,
            _subject: &str,
            _body: &str,
            _tid: Option<&str>,
            _message_id: Option<&str>,
            _in_reply_to: Option<&str>,
            _references: Option<&str>,
        ) -> Result<(), ChannelError> {
            self.calls.lock().unwrap().push(format!("{from}->{to}"));
            Ok(())
        }
    }

    struct FixedImap {
        emails: Vec<RawEmail>,
    }

    #[async_trait]
    impl ImapConnector for FixedImap {
        async fn fetch_page(
            &self,
            _since: DateTime<Utc>,
            _limit: usize,
            _progress: crate::connector::imap::ImapProgress,
        ) -> Result<ImapFetchPage, ChannelError> {
            Ok(ImapFetchPage {
                emails: self
                    .emails
                    .clone()
                    .into_iter()
                    .map(|e| SelectedMessage::Email(Box::new(e)))
                    .collect(),
                next_progress: crate::connector::imap::ImapProgress::default(),
            })
        }
    }

    /// Build a RawEmail with a single-address From, a stable IMAP external ID,
    /// and a trusted, self-aligned `Authentication-Results` header -- so
    /// callers that are exercising allowlist logic (not the auth gate itself)
    /// get an attributed baseline by default.
    fn make_email(from_addr: &str, imap_id: &str) -> RawEmail {
        RawEmail {
            uid: 1,
            imap_external_id: imap_id.to_string(),
            from_addrs: vec![from_addr.to_string()],
            sender_addr: None,
            to: vec!["me@example.com".to_string()],
            subject: "Hello".to_string(),
            date: None,
            body_text: Some("body text".to_string()),
            body_html: None,
            headers: HashMap::new(),
            authentication_results: vec![trusted_auth_header(from_addr)],
        }
    }

    /// Build a RawEmail with an explicit From address list and a trusted
    /// `Authentication-Results` header (aligned to the first From address, or
    /// to `example.com` when the list is empty).
    fn make_email_with_from_addrs(from_addrs: Vec<String>, imap_id: &str) -> RawEmail {
        let auth_header = from_addrs
            .first()
            .map(|addr| trusted_auth_header(addr))
            .unwrap_or_else(|| format!("{TEST_AUTHSERV_ID}; dmarc=pass header.from=example.com"));
        RawEmail {
            uid: 1,
            imap_external_id: imap_id.to_string(),
            from_addrs,
            sender_addr: None,
            to: vec!["me@example.com".to_string()],
            subject: "Hello".to_string(),
            date: None,
            body_text: Some("body text".to_string()),
            body_html: None,
            headers: HashMap::new(),
            authentication_results: vec![auth_header],
        }
    }

    fn build_channel(maintainer: &str, emails: Vec<RawEmail>) -> EmailChannel {
        build_channel_from(make_config(maintainer), emails)
    }

    fn build_channel_from(config: EmailChannelConfig, emails: Vec<RawEmail>) -> EmailChannel {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let smtp = SmtpSender::with_connector(RecordingSmtp {
            calls: calls.clone(),
        });
        let imap = ImapFetcher::with_connector(FixedImap { emails });
        EmailChannel::with_connectors(config, smtp, imap)
    }

    fn make_config_multi(maintainers: &[&str]) -> EmailChannelConfig {
        let mut config = make_config(maintainers[0]);
        config.maintainer_addresses = maintainers
            .iter()
            .map(|m| MailAddress::parse(m).unwrap())
            .collect();
        config
    }

    // --- Basic trait ---

    #[test]
    fn kind_is_email() {
        let ch = build_channel("maintainer@example.com", vec![]);
        assert_eq!(ch.kind(), "email");
    }

    // --- Authorization: authorized sender ---

    #[tokio::test]
    async fn authorized_sender_produces_envelope() {
        let ch = build_channel(
            "maintainer@example.com",
            vec![make_email("maintainer@example.com", "imap:test:0:1")],
        );
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        // external_id is now always the stable IMAP key, not Message-ID.
        assert_eq!(envs[0].external_id.as_deref(), Some("imap:test:0:1"));
        assert_eq!(envs[0].from, "email:maintainer@example.com");
    }

    #[tokio::test]
    async fn poll_extracts_wire_message_id_from_headers() {
        let mut email = make_email("maintainer@example.com", "imap:test:0:1");
        email
            .headers
            .insert("message-id".to_string(), "wire-abc@example.com".to_string());
        let ch = build_channel("maintainer@example.com", vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(
            envs[0].wire_message_id.as_deref(),
            Some("wire-abc@example.com"),
            "poll must surface the inbound email's own Message-ID as wire_message_id"
        );
    }

    #[tokio::test]
    async fn poll_wire_message_id_absent_when_no_header() {
        let ch = build_channel(
            "maintainer@example.com",
            vec![make_email("maintainer@example.com", "imap:test:0:1")],
        );
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(
            envs[0].wire_message_id, None,
            "no Message-ID header must leave wire_message_id unset, not fabricated"
        );
    }

    #[tokio::test]
    async fn poll_extracts_wire_references_from_headers() {
        let mut email = make_email("maintainer@example.com", "imap:test:0:1");
        email.headers.insert(
            "references".to_string(),
            "<grandparent1@example.com> <parent123@example.com>".to_string(),
        );
        let ch = build_channel("maintainer@example.com", vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(
            envs[0].wire_references.as_deref(),
            Some("<grandparent1@example.com> <parent123@example.com>"),
            "poll must surface the inbound email's own References chain as wire_references"
        );
    }

    #[tokio::test]
    async fn poll_preserves_multi_id_wire_references_through_real_parse() {
        // Regression: connector::imap::parse_raw_bytes must not drop a multi-id
        // References chain (mail-parser's `HeaderValue::TextList`) -- exercise the
        // REAL byte-parsing path here, not a hand-built RawEmail.headers map, since
        // that hand-built shortcut is exactly what let the live-path bug through.
        let raw = b"Authentication-Results: mx.example.com; dmarc=pass header.from=example.com\r\n\
                    From: maintainer@example.com\r\n\
                    To: me@example.com\r\n\
                    Subject: Multi-id References test\r\n\
                    References: <grandparent1@example.com> <grandparent2@example.com>\r\n\
                    \r\n\
                    body text";
        let email = parse_raw_bytes(1, raw, "imap.example.com", 4242)
            .expect("valid RFC 822 bytes must parse");
        let ch = build_channel("maintainer@example.com", vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(
            envs[0].wire_references.as_deref(),
            Some("grandparent1@example.com grandparent2@example.com"),
            "both ancestor ids parsed from raw bytes must survive into wire_references; \
             got envs={envs:?}"
        );
    }

    #[tokio::test]
    async fn poll_wire_references_absent_when_no_header() {
        let ch = build_channel(
            "maintainer@example.com",
            vec![make_email("maintainer@example.com", "imap:test:0:1")],
        );
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(
            envs[0].wire_references, None,
            "no References header must leave wire_references unset, not fabricated"
        );
    }

    #[tokio::test]
    async fn normalized_addr_spec_is_accepted() {
        // IMAP parsing strips display names before channel.rs sees the address.
        // from_addrs already contains the bare addr-spec.
        let ch = build_channel(
            "maintainer@example.com",
            vec![make_email("maintainer@example.com", "imap:test:0:1")],
        );
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1, "plain addr-spec must be accepted");
    }

    // --- Authorization: rejected senders (Fix 1) ---

    #[tokio::test]
    async fn unauthorized_sender_with_valid_auth_is_quarantined_off_allowlist() {
        // Domain authentication passes (attacker@example.com's own AR header is
        // trusted and aligned), but the sender is not on the maintainer
        // allowlist -- must be quarantined with reason off-allowlist, never
        // attributed to the (non-maintainer) sender or silently dropped.
        let ch = build_channel(
            "maintainer@example.com",
            vec![make_email("attacker@example.com", "imap:test:0:1")],
        );
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1, "must be stored as quarantine, not dropped");
        assert_eq!(envs[0].from, "email:quarantine");
        assert_eq!(
            envs[0]
                .metadata
                .get("quarantine_reason")
                .map(String::as_str),
            Some("off-allowlist")
        );
        assert_eq!(
            envs[0].metadata.get("quarantined").map(String::as_str),
            Some("true")
        );
    }

    #[tokio::test]
    async fn gmail_dot_variants_authorize_against_dotted_maintainer() {
        // Maintainer configured with a dotted Gmail; the client may deliver the
        // dotless canonical form, a googlemail alias, or a +tag. All are the same
        // Gmail mailbox and must authorize.
        for from in [
            "samrivera@gmail.com",
            "sam.rivera@gmail.com",
            "samrivera@googlemail.com",
            "sam.rivera+khive@gmail.com",
        ] {
            let ch = build_channel(
                "sam.rivera@gmail.com",
                vec![make_email(from, "imap:test:0:1")],
            );
            let envs = ch.poll(Utc::now()).await.unwrap();
            assert_eq!(envs.len(), 1, "gmail variant {from} must authorize");
        }
    }

    #[tokio::test]
    async fn non_gmail_dots_remain_significant() {
        // Dot-insensitivity is a Gmail-only rule; other providers treat dots as
        // significant, so a dotted variant of a non-Gmail maintainer fails the
        // allowlist half of the gate (quarantined, not attributed).
        let ch = build_channel(
            "sam.rivera@outlook.com",
            vec![make_email("samrivera@outlook.com", "imap:test:0:1")],
        );
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        assert_ne!(
            envs[0].from, "email:samrivera@outlook.com",
            "non-gmail dot-variant must NOT authorize"
        );
        assert_eq!(
            envs[0]
                .metadata
                .get("quarantine_reason")
                .map(String::as_str),
            Some("off-allowlist")
        );
    }

    #[tokio::test]
    async fn allowlist_authorizes_any_configured_maintainer() {
        // Multiple maintainers (e.g. a Gmail and an Outlook) may be registered;
        // a From matching any entry authorizes.
        let config = make_config_multi(&["primary@gmail.com", "second@outlook.com"]);
        let ch = build_channel_from(
            config,
            vec![make_email("second@outlook.com", "imap:test:0:1")],
        );
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1, "any allowlisted maintainer must authorize");
    }

    #[tokio::test]
    async fn multi_from_addresses_rejected() {
        // RFC 5322 permits multiple From addresses; we treat it as unauthorized
        // (quarantined off-allowlist, since domain auth on the first address passes).
        let ch = build_channel(
            "maintainer@example.com",
            vec![make_email_with_from_addrs(
                vec![
                    "maintainer@example.com".to_string(),
                    "attacker@example.com".to_string(),
                ],
                "imap:test:0:1",
            )],
        );
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(
            envs.len(),
            1,
            "multi-From message must be quarantined, not dropped"
        );
        assert_eq!(envs[0].from, "email:quarantine");
        assert_eq!(
            envs[0]
                .metadata
                .get("quarantine_reason")
                .map(String::as_str),
            Some("off-allowlist")
        );
    }

    #[tokio::test]
    async fn empty_from_list_rejected() {
        // With no From address there is no domain to align dmarc's
        // header.from against, so `dmarc_pass_aligned` (hardening #3, ADR-056
        // Amendment 2026-07-03) now correctly fails alignment and the
        // attribution gate catches this at the auth leg (dmarc-fail) before
        // ever reaching the sender allowlist -- still quarantined, just a
        // stricter (and more accurate) reason than the pre-hardening
        // off-allowlist path.
        let ch = build_channel(
            "maintainer@example.com",
            vec![make_email_with_from_addrs(vec![], "imap:test:0:1")],
        );
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(
            envs.len(),
            1,
            "message with no From address must be quarantined, not dropped"
        );
        assert_eq!(
            envs[0]
                .metadata
                .get("quarantine_reason")
                .map(String::as_str),
            Some("dmarc-fail")
        );
    }

    #[tokio::test]
    async fn sender_header_mismatch_rejected() {
        let mut email = make_email("maintainer@example.com", "imap:test:0:1");
        // Sender header claims a different mailbox -- reject.
        email.sender_addr = Some("attacker@example.com".to_string());
        let ch = build_channel("maintainer@example.com", vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(
            envs.len(),
            1,
            "Sender mismatch must be quarantined, not dropped"
        );
        assert_eq!(
            envs[0]
                .metadata
                .get("quarantine_reason")
                .map(String::as_str),
            Some("off-allowlist")
        );
    }

    #[tokio::test]
    async fn sender_header_matching_maintainer_accepted() {
        let mut email = make_email("maintainer@example.com", "imap:test:0:1");
        email.sender_addr = Some("maintainer@example.com".to_string());
        let ch = build_channel("maintainer@example.com", vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1, "matching Sender header must be accepted");
    }

    #[tokio::test]
    async fn reply_to_header_is_not_used_for_auth() {
        // Reply-To is irrelevant for authorization; only From (and optionally Sender) matter.
        let mut email = make_email("maintainer@example.com", "imap:test:0:1");
        email
            .headers
            .insert("reply-to".to_string(), "attacker@evil.com".to_string());
        let ch = build_channel("maintainer@example.com", vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(
            envs.len(),
            1,
            "Reply-To must not affect authorization; only From is checked"
        );
    }

    // --- Attribution gate: Authentication-Results (ADR-056 Amendment 2026-07-02) ---
    //
    // These exercise the REAL byte-parsing path (parse_raw_bytes), not hand-built
    // RawEmail/authentication_results literals, per the same real-parse discipline
    // as poll_preserves_multi_id_wire_references_through_real_parse above: a
    // synthetic header map would not exercise mail-parser's actual header
    // iteration and could hide a live-path bug the same way it did there.

    #[tokio::test]
    async fn spoofed_from_no_authentication_results_quarantines_auth_absent() {
        // The original #448 regression: a message whose From: claims to be the
        // maintainer, with no Authentication-Results header at all. Must be
        // quarantined -- and critically, the quarantine envelope's `from` must
        // NOT be the claimed maintainer address, or the vulnerability persists
        // one layer up (comm.ingest stamps from_actor straight from envelope.from).
        let raw = b"From: maintainer@example.com\r\n\
                    To: me@example.com\r\n\
                    Subject: spoofed, no auth at all\r\n\
                    \r\n\
                    body";
        let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
        let ch = build_channel("maintainer@example.com", vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        assert_ne!(
            envs[0].from, "email:maintainer@example.com",
            "quarantined mail must never carry the claimed maintainer address as `from`"
        );
        assert_eq!(envs[0].from, "email:quarantine");
        assert_eq!(
            envs[0]
                .metadata
                .get("quarantine_reason")
                .map(String::as_str),
            Some("auth-absent")
        );
        assert_eq!(
            envs[0]
                .metadata
                .get("quarantine_claimed_from")
                .map(String::as_str),
            Some("maintainer@example.com"),
            "the claimed From is preserved in metadata for review, but not as `from`"
        );
    }

    #[tokio::test]
    async fn multi_authentication_results_topmost_trusted_id_wins_over_forged_below() {
        // Two Authentication-Results headers share the trusted authserv-id: the
        // genuine one (dmarc=pass) is topmost (added last, by the final trusted
        // hop); a forged one (dmarc=fail) was injected below it. Topmost wins,
        // per the ADR's trusted-header selection rule -- the genuine stamp is
        // used regardless of what a lower, same-id header claims.
        let raw = b"Authentication-Results: mx.example.com; dmarc=pass header.from=example.com\r\n\
                    Authentication-Results: mx.example.com; dmarc=fail header.from=example.com\r\n\
                    From: maintainer@example.com\r\n\
                    To: me@example.com\r\n\
                    Subject: genuine on top, forged below\r\n\
                    \r\n\
                    body";
        let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
        let ch = build_channel("maintainer@example.com", vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(
            envs[0].from, "email:maintainer@example.com",
            "topmost genuine dmarc=pass must win over a same-id forged header below it"
        );
    }

    #[tokio::test]
    async fn single_authresults_with_trusted_id_is_mechanically_trusted_operational_precondition() {
        // OPERATIONAL-PRECONDITION BOUNDARY, not a bug: when exactly one
        // Authentication-Results header is present and it carries the
        // configured trusted authserv-id, the gate has no way to distinguish
        // "the receiving MTA genuinely stamped this" from "an attacker injected
        // a single header claiming our trusted id before the (misconfigured or
        // absent) receiving MTA had a chance to strip it and add its own".
        // Per ADR-056 Amendment 2026-07-02 ("Trusted-header selection"), that
        // stripping is a deployment-time responsibility of the receiving
        // boundary, not something this parser re-verifies from message content
        // (doing so would mean deriving trust from the very content the header
        // is supposed to authenticate). Given that precondition holds in
        // production, this header is mechanically trusted.
        let raw = b"Authentication-Results: mx.example.com; dmarc=pass header.from=example.com\r\n\
                    From: maintainer@example.com\r\n\
                    To: me@example.com\r\n\
                    Subject: single AR header, trusted id\r\n\
                    \r\n\
                    body";
        let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
        let ch = build_channel("maintainer@example.com", vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].from, "email:maintainer@example.com");
    }

    #[tokio::test]
    async fn authresults_with_untrusted_authserv_id_is_ignored_quarantines_auth_absent() {
        // A header is present, but its authserv-id does not match this
        // deployment's configured trust anchor -- it must be ignored entirely
        // (never treated as a partial signal), leaving no trusted header, hence
        // auth-absent.
        let raw =
            b"Authentication-Results: forged-mx.evil.com; dmarc=pass header.from=example.com\r\n\
                    From: maintainer@example.com\r\n\
                    To: me@example.com\r\n\
                    Subject: wrong authserv-id\r\n\
                    \r\n\
                    body";
        let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
        let ch = build_channel("maintainer@example.com", vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].from, "email:quarantine");
        assert_eq!(
            envs[0]
                .metadata
                .get("quarantine_reason")
                .map(String::as_str),
            Some("auth-absent")
        );
    }

    #[tokio::test]
    async fn quoted_reason_semicolon_forging_dmarc_pass_does_not_bypass_gate() {
        // Regression for review #496 Finding 1, driven through the REAL byte-parsing
        // path (parse_raw_bytes), not a hand-built AuthResults: a quoted reason=
        // pvalue containing "; dmarc=pass; " must never be split into a separate
        // dmarc method -- the gate must see the genuine spf=fail result and
        // quarantine, never attribute to the maintainer.
        let raw = b"Authentication-Results: mx.example.com; spf=fail reason=\"remote said; dmarc=pass; still fail\" smtp.mailfrom=attacker.net\r\n\
                    From: maintainer@example.com\r\n\
                    To: me@example.com\r\n\
                    Subject: quoted reason attack\r\n\
                    \r\n\
                    body";
        let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
        let ch = build_channel("maintainer@example.com", vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(
            envs[0].from, "email:quarantine",
            "a forged dmarc=pass hidden inside a quoted reason pvalue must never attribute mail"
        );
        assert_eq!(
            envs[0]
                .metadata
                .get("quarantine_reason")
                .map(String::as_str),
            Some("dmarc-fail")
        );
    }

    #[tokio::test]
    async fn quoted_whitespace_smtp_mailfrom_injection_does_not_bypass_gate() {
        // Regression for the #501 property-tokenizer hardening, driven through
        // the REAL byte-parsing path (parse_raw_bytes): a quoted
        // smtp.mailfrom= pvalue containing embedded whitespace and an
        // attacker-controlled inner "smtp.mailfrom=example.com" must never be
        // shattered into a separate, alignment-winning property token. The
        // real envelope domain (evil.com) must be what the gate sees, so this
        // must quarantine as unaligned, never attribute to the maintainer.
        let raw = b"Authentication-Results: mx.example.com; spf=pass smtp.mailfrom=\"attacker smtp.mailfrom=example.com \"@evil.com\r\n\
                    From: maintainer@example.com\r\n\
                    To: me@example.com\r\n\
                    Subject: quoted whitespace property injection\r\n\
                    \r\n\
                    body";
        let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
        let ch = build_channel("maintainer@example.com", vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(
            envs[0].from, "email:quarantine",
            "a forged smtp.mailfrom hidden inside a quoted pvalue's embedded whitespace must never attribute mail"
        );
        assert_eq!(
            envs[0]
                .metadata
                .get("quarantine_reason")
                .map(String::as_str),
            Some("unaligned")
        );
    }

    #[tokio::test]
    async fn spf_pass_unaligned_envelope_from_quarantines_unaligned() {
        let raw = b"Authentication-Results: mx.example.com; spf=pass smtp.mailfrom=alice@attacker.net\r\n\
                    From: maintainer@example.com\r\n\
                    To: me@example.com\r\n\
                    Subject: spf pass, mismatched envelope domain\r\n\
                    \r\n\
                    body";
        let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
        let ch = build_channel("maintainer@example.com", vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].from, "email:quarantine");
        assert_eq!(
            envs[0]
                .metadata
                .get("quarantine_reason")
                .map(String::as_str),
            Some("unaligned")
        );
    }

    #[tokio::test]
    async fn dkim_pass_aligned_d_domain_is_attributed() {
        let raw = b"Authentication-Results: mx.example.com; dkim=pass header.d=example.com header.s=sel1\r\n\
                    From: maintainer@example.com\r\n\
                    To: me@example.com\r\n\
                    Subject: dkim pass, aligned d=\r\n\
                    \r\n\
                    body";
        let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
        let ch = build_channel("maintainer@example.com", vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].from, "email:maintainer@example.com");
    }

    // --- EXO no-authserv-id trust anchor, end-to-end (ADR-056 Amendment 2026-07-03) ---

    #[tokio::test]
    async fn exo_no_authserv_id_fixture_end_to_end_is_attributed() {
        // Real Exchange Online header shape (verbatim values from the live
        // fixture `raw_headers.txt`): an ARC-Authentication-Results
        // header (must be ignored -- collecting/trusting ARC is the rejected
        // Shape B, out of scope) precedes the plain Authentication-Results
        // header EXO stamps on its own internal hop, which carries no
        // authserv-id at all. In TopmostNoAuthservId mode this must attribute
        // cleanly through the real byte-parsing path.
        let raw = b"ARC-Authentication-Results: i=2; mx.microsoft.com 1; spf=pass smtp.mailfrom=gmail.com; dmarc=pass header.from=gmail.com; dkim=pass header.d=gmail.com\r\n\
                    Authentication-Results: spf=pass (sender IP is 2607:f8b0:4864:20::1129) smtp.mailfrom=gmail.com; dkim=pass (signature was verified) header.d=gmail.com;dmarc=pass action=none header.from=gmail.com;compauth=pass reason=100\r\n\
                    From: Example Sender <sender@gmail.com>\r\n\
                    To: Example Recipient <recipient@example.com>\r\n\
                    Subject: Khive email subject disappears into body\r\n\
                    \r\n\
                    body text";
        let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();

        let mut config = make_config("sender@gmail.com");
        config.trust_anchor = TrustAnchor::TopmostNoAuthservId;
        let ch = build_channel_from(config, vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(
            envs[0].from, "email:sender@gmail.com",
            "the real EXO no-authserv-id header must attribute cleanly in TopmostNoAuthservId mode"
        );
    }

    #[tokio::test]
    async fn exo_mode_topmost_carrying_an_authserv_id_quarantines_auth_absent() {
        // If the topmost Authentication-Results unexpectedly carries an
        // authserv-id while this deployment is configured for
        // TopmostNoAuthservId mode, that violates the invariant that EXO's
        // own stamp is topmost and unadorned (Microsoft could start emitting
        // one, or an attacker's forged header floated to the top) --
        // quarantine rather than trust it.
        let raw = b"Authentication-Results: mx.microsoft.com; dmarc=pass header.from=gmail.com\r\n\
                    From: Example Sender <sender@gmail.com>\r\n\
                    To: Example Recipient <recipient@example.com>\r\n\
                    Subject: forged topmost carries an id\r\n\
                    \r\n\
                    body";
        let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
        let mut config = make_config("sender@gmail.com");
        config.trust_anchor = TrustAnchor::TopmostNoAuthservId;
        let ch = build_channel_from(config, vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].from, "email:quarantine");
        assert_eq!(
            envs[0]
                .metadata
                .get("quarantine_reason")
                .map(String::as_str),
            Some("auth-absent")
        );
    }

    #[tokio::test]
    async fn quarantine_store_off_drops_unattributed_message() {
        let mut config = make_config("maintainer@example.com");
        config.quarantine_store = false;
        let raw = b"From: maintainer@example.com\r\n\
                    To: me@example.com\r\n\
                    Subject: no auth, quarantine store off\r\n\
                    \r\n\
                    body";
        let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
        let ch = build_channel_from(config, vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert!(
            envs.is_empty(),
            "quarantine_store=false must drop the message, not store it"
        );
    }

    // --- Batch isolation (Fix 3) ---

    #[tokio::test]
    async fn bad_message_in_batch_does_not_abort_poll() {
        let ch = build_channel(
            "maintainer@example.com",
            vec![
                // First message: unauthorized -- must be quarantined, not abort the batch.
                make_email("attacker@example.com", "imap:test:0:1"),
                // Second message: authorized -- must be attributed.
                make_email("maintainer@example.com", "imap:test:0:2"),
            ],
        );
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 2, "both messages must produce an envelope");

        let quarantined = envs
            .iter()
            .find(|e| e.external_id.as_deref() == Some("imap:test:0:1"))
            .expect("unauthorized message must still be present, quarantined");
        assert_eq!(quarantined.from, "email:quarantine");
        assert_eq!(
            quarantined
                .metadata
                .get("quarantine_reason")
                .map(String::as_str),
            Some("off-allowlist")
        );

        let attributed = envs
            .iter()
            .find(|e| e.external_id.as_deref() == Some("imap:test:0:2"))
            .expect("authorized message must be attributed");
        assert_eq!(attributed.from, "email:maintainer@example.com");
    }

    // --- SMTP send path ---

    #[tokio::test]
    async fn send_strips_email_prefix() {
        let config = make_config("maintainer@example.com");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let smtp = SmtpSender::with_connector(RecordingSmtp {
            calls: calls.clone(),
        });
        let imap = ImapFetcher::with_connector(FixedImap { emails: vec![] });
        let ch = EmailChannel::with_connectors(config, smtp, imap);

        let env = ChannelEnvelope::new("email:from@example.com", "email:to@example.com", "hello");
        ch.send(env).await.unwrap();

        let recorded = calls.lock().unwrap();
        assert_eq!(recorded[0], "from@example.com->to@example.com");
    }

    #[tokio::test]
    async fn send_forwards_in_reply_to_to_connector() {
        struct CapturingSmtp {
            captured: Arc<Mutex<Vec<Option<String>>>>,
        }

        #[async_trait]
        impl SmtpConnector for CapturingSmtp {
            async fn deliver(
                &self,
                _from: &str,
                _to: &str,
                _subject: &str,
                _body: &str,
                _tid: Option<&str>,
                _message_id: Option<&str>,
                in_reply_to: Option<&str>,
                _references: Option<&str>,
            ) -> Result<(), ChannelError> {
                self.captured
                    .lock()
                    .unwrap()
                    .push(in_reply_to.map(|s| s.to_string()));
                Ok(())
            }
        }

        let config = make_config("maintainer@example.com");
        let captured = Arc::new(Mutex::new(Vec::new()));
        let smtp = SmtpSender::with_connector(CapturingSmtp {
            captured: captured.clone(),
        });
        let imap = ImapFetcher::with_connector(FixedImap { emails: vec![] });
        let ch = EmailChannel::with_connectors(config, smtp, imap);

        let env = ChannelEnvelope::new("email:from@example.com", "email:to@example.com", "hello")
            .with_in_reply_to("<parent123@example.com>");
        ch.send(env).await.unwrap();

        let vals = captured.lock().unwrap();
        assert_eq!(vals[0].as_deref(), Some("<parent123@example.com>"));
    }

    #[tokio::test]
    async fn send_forwards_references_to_connector() {
        struct CapturingSmtp {
            captured: Arc<Mutex<Vec<Option<String>>>>,
        }

        #[async_trait]
        impl SmtpConnector for CapturingSmtp {
            async fn deliver(
                &self,
                _from: &str,
                _to: &str,
                _subject: &str,
                _body: &str,
                _tid: Option<&str>,
                _message_id: Option<&str>,
                _in_reply_to: Option<&str>,
                references: Option<&str>,
            ) -> Result<(), ChannelError> {
                self.captured
                    .lock()
                    .unwrap()
                    .push(references.map(|s| s.to_string()));
                Ok(())
            }
        }

        let config = make_config("maintainer@example.com");
        let captured = Arc::new(Mutex::new(Vec::new()));
        let smtp = SmtpSender::with_connector(CapturingSmtp {
            captured: captured.clone(),
        });
        let imap = ImapFetcher::with_connector(FixedImap { emails: vec![] });
        let ch = EmailChannel::with_connectors(config, smtp, imap);

        let env = ChannelEnvelope::new("email:from@example.com", "email:to@example.com", "hello")
            .with_in_reply_to("<parent123@example.com>")
            .with_references("<grandparent1@example.com> <parent123@example.com>");
        ch.send(env).await.unwrap();

        let vals = captured.lock().unwrap();
        assert_eq!(
            vals[0].as_deref(),
            Some("<grandparent1@example.com> <parent123@example.com>")
        );
    }

    // --- checkpoint decode (issue #449) ---

    #[test]
    fn source_mismatch_drops_stored_progress() {
        let ch = build_channel_from(make_config("maintainer@example.com"), vec![]);
        let stored = StoredChannelCheckpoint {
            checkpoint: ChannelCheckpoint {
                source: "imap+tls:other.example.com:993:other@example.com:INBOX".to_string(),
                generation: 42,
                high_water: Some(10),
            },
            committed_at: Utc::now(),
        };
        let (progress, committed_at) = ch.decode_progress(Some(&stored)).unwrap();
        assert_eq!(
            progress,
            ImapProgress::default(),
            "a source mismatch (different host/port/mailbox) must drop stored progress"
        );
        assert!(
            committed_at.is_none(),
            "a source mismatch must not surface the other source's recovery floor either"
        );
    }

    #[test]
    fn invalid_persisted_imap_width_or_zero_fails_closed() {
        let ch = build_channel_from(make_config("maintainer@example.com"), vec![]);
        let source = ch.checkpoint_source();

        let overflowing_generation = StoredChannelCheckpoint {
            checkpoint: ChannelCheckpoint {
                source: source.clone(),
                generation: u64::from(u32::MAX) + 1,
                high_water: None,
            },
            committed_at: Utc::now(),
        };
        assert!(
            ch.decode_progress(Some(&overflowing_generation)).is_err(),
            "a generation wider than u32 must fail closed, not truncate"
        );

        let zero_generation = StoredChannelCheckpoint {
            checkpoint: ChannelCheckpoint {
                source: source.clone(),
                generation: 0,
                high_water: None,
            },
            committed_at: Utc::now(),
        };
        assert!(
            ch.decode_progress(Some(&zero_generation)).is_err(),
            "a zero generation must fail closed, not silently reset to empty progress"
        );

        let overflowing_high_water = StoredChannelCheckpoint {
            checkpoint: ChannelCheckpoint {
                source,
                generation: 1,
                high_water: Some(u64::from(u32::MAX) + 1),
            },
            committed_at: Utc::now(),
        };
        assert!(
            ch.decode_progress(Some(&overflowing_high_water)).is_err(),
            "a high_water wider than u32 must fail closed"
        );
    }

    #[test]
    fn strip_kind_prefix_removes_prefix() {
        assert_eq!(
            strip_kind_prefix("email:user@example.com", "email"),
            "user@example.com"
        );
        assert_eq!(
            strip_kind_prefix("user@example.com", "email"),
            "user@example.com"
        );
    }

    // --- poll_page / poll integration (issue #449) ---
    //
    // `select_uid_page`/`next_progress` are module-private to `connector::imap`
    // and covered directly by that module's own tests. The mock below
    // replicates the same sort/dedup/high-water-filter/truncate contract
    // inline so these tests can drive the actual `EmailChannel::poll_page`/
    // `poll` orchestration (disposition, checkpoint encode/decode, source
    // binding), not just the connector-level helpers in isolation.

    struct KeysetMockImap {
        validity: NonZeroU32,
        all_uids: Vec<u32>,
    }

    impl KeysetMockImap {
        fn select(&self, limit: usize, progress: ImapProgress) -> Vec<NonZeroU32> {
            let mut uids = self.all_uids.clone();
            uids.sort_unstable();
            uids.dedup();
            if progress.uid_validity == Some(self.validity) {
                if let Some(high_water) = progress.last_seen_uid {
                    uids.retain(|&u| u > high_water.get());
                }
            }
            uids.truncate(limit);
            uids.into_iter()
                .map(|u| NonZeroU32::new(u).unwrap())
                .collect()
        }

        fn email_for(&self, uid: NonZeroU32) -> RawEmail {
            let mut email = make_email(
                "sender@example.com",
                &format!(
                    "imap:mail.example.com:{}:{}",
                    self.validity.get(),
                    uid.get()
                ),
            );
            email.uid = uid.get();
            email
        }
    }

    #[async_trait]
    impl ImapConnector for KeysetMockImap {
        async fn fetch_page(
            &self,
            _since: DateTime<Utc>,
            limit: usize,
            progress: ImapProgress,
        ) -> Result<ImapFetchPage, ChannelError> {
            let selected = self.select(limit, progress);
            let emails: Vec<SelectedMessage> = selected
                .iter()
                .map(|&u| SelectedMessage::Email(Box::new(self.email_for(u))))
                .collect();
            let next_progress = match selected.iter().map(|u| u.get()).max() {
                Some(max) => ImapProgress {
                    uid_validity: Some(self.validity),
                    last_seen_uid: NonZeroU32::new(max),
                },
                None if progress.uid_validity == Some(self.validity) => progress,
                None => ImapProgress {
                    uid_validity: Some(self.validity),
                    last_seen_uid: None,
                },
            };
            Ok(ImapFetchPage {
                emails,
                next_progress,
            })
        }
    }

    fn build_keyset_channel(mailbox: &str, validity: u32, all_uids: Vec<u32>) -> EmailChannel {
        let mut config = make_config("maintainer@example.com");
        config.mailbox = mailbox.to_string();
        config.username = mailbox.to_string();
        let smtp = SmtpSender::with_connector(RecordingSmtp {
            calls: Arc::new(Mutex::new(Vec::new())),
        });
        let imap = ImapFetcher::with_connector(KeysetMockImap {
            validity: NonZeroU32::new(validity).unwrap(),
            all_uids,
        });
        EmailChannel::with_connectors(config, smtp, imap)
    }

    #[tokio::test]
    async fn poll_page_resumes_from_persisted_checkpoint_across_restart() {
        let since = Utc::now();

        // First "process": drains the initial backlog 1..=5 and produces a
        // checkpoint candidate.
        let first = build_keyset_channel("a@example.com", 7, vec![1, 2, 3, 4, 5]);
        let page1 = first.poll_page(since, None).await.unwrap();
        assert_eq!(page1.envelopes.len(), 5);
        let checkpoint = page1
            .next_checkpoint
            .expect("non-empty page must checkpoint");
        let stored = StoredChannelCheckpoint {
            checkpoint,
            committed_at: Utc::now(),
        };

        // A brand-new `EmailChannel` instance (simulating a process restart:
        // no shared in-memory state) sees the same 1..=5 backlog plus one
        // newly-arrived UID 6.
        let restarted = build_keyset_channel("a@example.com", 7, vec![1, 2, 3, 4, 5, 6]);
        let page2 = restarted.poll_page(since, Some(&stored)).await.unwrap();
        assert_eq!(
            page2.envelopes.len(),
            1,
            "a fresh instance given the persisted checkpoint must fetch zero \
             already-seen messages"
        );
        assert_eq!(
            page2.envelopes[0].external_id.as_deref(),
            Some("imap:mail.example.com:7:6"),
            "the fresh instance must correctly pick up the newly-arrived UID"
        );
        assert_eq!(
            page2.next_checkpoint.unwrap().high_water,
            Some(6),
            "checkpoint must advance to the newly-arrived UID"
        );
    }

    #[tokio::test]
    async fn poll_page_two_mailboxes_have_independent_cursors() {
        let since = Utc::now();
        let mailbox_a_checkpoint = StoredChannelCheckpoint {
            checkpoint: ChannelCheckpoint {
                source: "imap+tls:imap.example.com:993:a@example.com:INBOX".to_string(),
                generation: 99,
                high_water: Some(50),
            },
            committed_at: Utc::now(),
        };

        // Mailbox B has its own small, low-numbered backlog untouched by A's
        // checkpoint.
        let mailbox_b = build_keyset_channel("b@example.com", 5, vec![1, 2, 3]);
        let page = mailbox_b
            .poll_page(since, Some(&mailbox_a_checkpoint))
            .await
            .unwrap();

        assert_eq!(
            page.envelopes.len(),
            3,
            "mailbox A's checkpoint must be rejected by B's source check, leaving \
             B's own backlog untouched"
        );
        assert_eq!(page.next_checkpoint.unwrap().generation, 5);
    }

    #[tokio::test]
    async fn poll_page_retried_call_with_uncommitted_checkpoint_is_idempotent() {
        let since = Utc::now();
        let ch = build_keyset_channel("a@example.com", 9, vec![1, 2, 3]);

        let first = ch.poll_page(since, None).await.unwrap();
        // Simulate a retry after a crash between fetch and cursor commit: the
        // coordinator calls poll_page again with the same (uncommitted) `None`.
        let retry = ch.poll_page(since, None).await.unwrap();

        let ids = |p: &ChannelPollPage| {
            p.envelopes
                .iter()
                .map(|e| e.external_id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(&first),
            ids(&retry),
            "retrying with the same uncommitted checkpoint must return \
             byte-identical envelopes"
        );
        assert_eq!(
            first.next_checkpoint.unwrap(),
            retry.next_checkpoint.unwrap(),
            "retrying with the same uncommitted checkpoint must return an \
             identical checkpoint candidate"
        );
    }

    #[tokio::test]
    async fn poll_page_quarantine_disabled_drop_still_advances_checkpoint() {
        let since = Utc::now();
        let mut config = make_config("maintainer@example.com");
        config.quarantine_store = false;
        let smtp = SmtpSender::with_connector(RecordingSmtp {
            calls: Arc::new(Mutex::new(Vec::new())),
        });

        // A message with no Authentication-Results header fails the gate
        // (AuthAbsent) and, with quarantine_store disabled, is dropped rather
        // than emitted as an envelope.
        let mut gate_failing = make_email("forged@example.com", "imap:mail.example.com:3:1");
        gate_failing.authentication_results.clear();
        gate_failing.uid = 1;

        struct SingleGateFailingImap {
            email: RawEmail,
        }
        #[async_trait]
        impl ImapConnector for SingleGateFailingImap {
            async fn fetch_page(
                &self,
                _since: DateTime<Utc>,
                _limit: usize,
                _progress: ImapProgress,
            ) -> Result<ImapFetchPage, ChannelError> {
                Ok(ImapFetchPage {
                    emails: vec![SelectedMessage::Email(Box::new(self.email.clone()))],
                    next_progress: ImapProgress {
                        uid_validity: NonZeroU32::new(3),
                        last_seen_uid: NonZeroU32::new(self.email.uid),
                    },
                })
            }
        }
        let imap = ImapFetcher::with_connector(SingleGateFailingImap {
            email: gate_failing,
        });
        let ch = EmailChannel::with_connectors(config, smtp, imap);

        let page = ch.poll_page(since, None).await.unwrap();
        assert!(
            page.envelopes.is_empty(),
            "a gate-failing message with quarantine_store disabled must be dropped, \
             not emitted"
        );
        assert_eq!(
            page.next_checkpoint.unwrap().high_water,
            Some(1),
            "the checkpoint must still advance past the dropped message's UID -- \
             checkpoint advancement is bound to the selected UID page, not to \
             which envelopes disposition actually emits"
        );
    }

    /// khive #449 High follow-up (Medium-2): the connector-level
    /// `one_poison_uid_does_not_starve_51_later_valid_uids_and_cursor_passes_it`
    /// test in `imap.rs` only proves `process_selected_page` assigns the
    /// poison UID a `SelectedMessage::Malformed` disposition -- it never
    /// proves that disposition survives into the actual `ChannelEnvelope`
    /// `EmailChannel::poll_page` hands to `comm.ingest`. Drives a
    /// `SelectedMessage::Malformed` entry (as the IMAP connector would
    /// return it for an unparseable UID) through `poll_page`'s public
    /// pipeline and asserts the resulting envelope carries the stable
    /// `imap:{host}:{uidvalidity}:{uid}` external ID plus durable quarantine
    /// metadata -- exactly what the daemon's ingest/query layer needs to
    /// prove a durable quarantine record, not just an intermediate value.
    #[tokio::test]
    async fn poll_page_malformed_uid_produces_a_stable_external_id_and_quarantine_metadata() {
        let since = Utc::now();
        let config = make_config("maintainer@example.com");
        let smtp = SmtpSender::with_connector(RecordingSmtp {
            calls: Arc::new(Mutex::new(Vec::new())),
        });

        struct PoisonUidImap {
            good: RawEmail,
        }
        #[async_trait]
        impl ImapConnector for PoisonUidImap {
            async fn fetch_page(
                &self,
                _since: DateTime<Utc>,
                _limit: usize,
                _progress: ImapProgress,
            ) -> Result<ImapFetchPage, ChannelError> {
                Ok(ImapFetchPage {
                    emails: vec![
                        SelectedMessage::Malformed {
                            uid: 1,
                            imap_external_id: "imap:imap.example.com:4:1".to_string(),
                            reason: MalformedReason::MissingBody,
                        },
                        SelectedMessage::Email(Box::new(self.good.clone())),
                    ],
                    next_progress: ImapProgress {
                        uid_validity: NonZeroU32::new(4),
                        last_seen_uid: NonZeroU32::new(2),
                    },
                })
            }
        }

        let mut good = make_email("sender@example.com", "imap:imap.example.com:4:2");
        good.uid = 2;
        let imap = ImapFetcher::with_connector(PoisonUidImap { good });
        let ch = EmailChannel::with_connectors(config, smtp, imap);

        let page = ch.poll_page(since, None).await.unwrap();
        assert_eq!(
            page.envelopes.len(),
            2,
            "the poison UID must still produce a durable quarantine envelope \
             alongside the valid message, not a dropped/short page"
        );

        let quarantined = page
            .envelopes
            .iter()
            .find(|e| e.external_id.as_deref() == Some("imap:imap.example.com:4:1"))
            .expect(
                "the malformed UID's stable external_id must be present on the \
                 envelope actually handed to comm.ingest",
            );
        assert_eq!(
            quarantined.from, "email:quarantine",
            "a malformed UID must be attributed to the fixed quarantine marker"
        );
        assert_eq!(
            quarantined.metadata.get("quarantined").map(String::as_str),
            Some("true"),
            "the envelope must carry durable quarantine metadata for comm.ingest \
             to persist"
        );
        assert_eq!(
            quarantined
                .metadata
                .get("quarantine_reason")
                .map(String::as_str),
            Some("missing-body"),
            "the quarantine reason must be preserved onto the persisted envelope"
        );

        assert_eq!(
            page.next_checkpoint.unwrap().high_water,
            Some(2),
            "the checkpoint candidate must advance past the poison UID"
        );
    }

    #[tokio::test]
    async fn poll_drains_75_same_day_backlog_across_repeated_calls_via_production_entrypoint() {
        // Drives the actual production entrypoint (`EmailChannel::poll`, the
        // literal call `serve.rs` makes) with the issue's own numbers: 75
        // same-day UIDs, page limit 50, shuffled input mirroring HashSet
        // iteration-order noise.
        let mut all_uids: Vec<u32> = (1..=75).rev().collect();
        all_uids.extend(1..=10); // duplicates
        let ch = build_keyset_channel("a@example.com", 42, all_uids);

        let since = Utc::now();
        let page1 = ch.poll(since).await.unwrap();
        assert_eq!(page1.len(), 50, "first poll must return exactly 50");
        let page2 = ch.poll(since).await.unwrap();
        assert_eq!(
            page2.len(),
            25,
            "second poll must return the remaining 25, not a repeat of the first 50"
        );
        let page3 = ch.poll(since).await.unwrap();
        assert!(
            page3.is_empty(),
            "third poll must be empty; backlog fully drained"
        );

        let mut all_ids: Vec<String> = page1
            .iter()
            .chain(page2.iter())
            .map(|e| e.external_id.clone().unwrap())
            .collect();
        all_ids.sort();
        all_ids.dedup();
        assert_eq!(
            all_ids.len(),
            75,
            "all 75 external IDs must be distinct across both calls -- none skipped, \
             none repeated"
        );
    }
}
