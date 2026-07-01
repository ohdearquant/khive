//! `EmailChannel` — implements the `Channel` trait for email transport.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use khive_channel::{Channel, ChannelEnvelope, ChannelError};
use tracing::{debug, warn};

use crate::config::{EmailAuth, EmailChannelConfig};
use crate::connector::imap::ImapFetcher;
use crate::connector::smtp::SmtpSender;
use crate::connector::{MailAddress, RawEmail};
use crate::oauth::TokenProvider;

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

    /// The configured mailbox address (e.g. `leo@khive.ai`).
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

    /// Convert a `RawEmail` to a `ChannelEnvelope`, validating the sender.
    fn to_envelope(&self, email: RawEmail) -> Result<ChannelEnvelope, ChannelError> {
        self.check_sender(&email.from_addrs, email.sender_addr.as_deref())?;

        // Safe: check_sender verified exactly one entry.
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

        Ok(env)
    }
}

#[async_trait]
impl Channel for EmailChannel {
    fn kind(&self) -> &'static str {
        "email"
    }

    async fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
        let from = strip_kind_prefix(&envelope.from, "email");
        let to = strip_kind_prefix(&envelope.to, "email");
        let subject = envelope.subject.as_deref().unwrap_or("(no subject)");
        let thread_id = envelope.correlation_external_id.as_deref();
        let message_id = envelope.message_id.as_deref();

        debug!(from, to, subject, "email send");
        self.smtp
            .send(from, to, subject, &envelope.content, thread_id, message_id)
            .await
    }

    async fn poll(&self, since: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
        let raw = self.imap.fetch_since(since, 50).await?;
        let mut envelopes = Vec::new();
        for email in raw {
            let uid = email.uid;
            match self.to_envelope(email) {
                Ok(env) => envelopes.push(env),
                Err(e) => {
                    // Log the IMAP UID and the (address-free) error reason so a rejected
                    // inbound is observable in the daemon log. ChannelError messages are
                    // constructed without addresses or credentials, so `%e` is log-safe.
                    warn!(uid, error = %e, "skipping message: validation failed");
                }
            }
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
    use crate::config::EmailAuth;
    use crate::connector::imap::{ImapConnector, ImapFetcher};
    use crate::connector::smtp::{SmtpConnector, SmtpSender};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

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
        }
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
        async fn fetch_since(
            &self,
            _since: DateTime<Utc>,
            _limit: usize,
        ) -> Result<Vec<RawEmail>, ChannelError> {
            Ok(self.emails.clone())
        }
    }

    /// Build a RawEmail with a single-address From and a stable IMAP external ID.
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
        }
    }

    /// Build a RawEmail with an explicit From address list.
    fn make_email_with_from_addrs(from_addrs: Vec<String>, imap_id: &str) -> RawEmail {
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
    async fn unauthorized_sender_is_silently_skipped() {
        let ch = build_channel(
            "maintainer@example.com",
            vec![make_email("attacker@example.com", "imap:test:0:1")],
        );
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert!(envs.is_empty(), "unauthorized From must be dropped");
    }

    #[tokio::test]
    async fn gmail_dot_variants_authorize_against_dotted_maintainer() {
        // Maintainer configured with a dotted Gmail; the client may deliver the
        // dotless canonical form, a googlemail alias, or a +tag. All are the same
        // Gmail mailbox and must authorize.
        for from in [
            "quantoceanli@gmail.com",
            "quantocean.li@gmail.com",
            "quantoceanli@googlemail.com",
            "quantocean.li+khive@gmail.com",
        ] {
            let ch = build_channel(
                "quantocean.li@gmail.com",
                vec![make_email(from, "imap:test:0:1")],
            );
            let envs = ch.poll(Utc::now()).await.unwrap();
            assert_eq!(envs.len(), 1, "gmail variant {from} must authorize");
        }
    }

    #[tokio::test]
    async fn non_gmail_dots_remain_significant() {
        // Dot-insensitivity is a Gmail-only rule; other providers treat dots as
        // significant, so a dotted variant of a non-Gmail maintainer is rejected.
        let ch = build_channel(
            "quantocean.li@outlook.com",
            vec![make_email("quantoceanli@outlook.com", "imap:test:0:1")],
        );
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert!(envs.is_empty(), "non-gmail dot-variant must NOT authorize");
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
        // RFC 5322 permits multiple From addresses; we treat it as unauthorized.
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
        assert!(envs.is_empty(), "multi-From message must be rejected");
    }

    #[tokio::test]
    async fn empty_from_list_rejected() {
        let ch = build_channel(
            "maintainer@example.com",
            vec![make_email_with_from_addrs(vec![], "imap:test:0:1")],
        );
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert!(
            envs.is_empty(),
            "message with no From address must be rejected"
        );
    }

    #[tokio::test]
    async fn sender_header_mismatch_rejected() {
        let mut email = make_email("maintainer@example.com", "imap:test:0:1");
        // Sender header claims a different mailbox -- reject.
        email.sender_addr = Some("attacker@example.com".to_string());
        let ch = build_channel("maintainer@example.com", vec![email]);
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert!(envs.is_empty(), "Sender mismatch must be rejected");
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

    // --- Batch isolation (Fix 3) ---

    #[tokio::test]
    async fn bad_message_in_batch_does_not_abort_poll() {
        let ch = build_channel(
            "maintainer@example.com",
            vec![
                // First message: unauthorized -- should be skipped.
                make_email("attacker@example.com", "imap:test:0:1"),
                // Second message: authorized -- must be returned.
                make_email("maintainer@example.com", "imap:test:0:2"),
            ],
        );
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(
            envs.len(),
            1,
            "only the authorized message must be returned"
        );
        assert_eq!(
            envs[0].external_id.as_deref(),
            Some("imap:test:0:2"),
            "the authorized message must be the one returned"
        );
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
}
