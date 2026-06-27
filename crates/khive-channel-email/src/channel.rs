//! `EmailChannel` — implements the `Channel` trait for email transport.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use khive_channel::{Channel, ChannelEnvelope, ChannelError};
use tracing::{debug, warn};

use crate::config::EmailChannelConfig;
use crate::connector::imap::ImapFetcher;
use crate::connector::smtp::SmtpSender;
use crate::connector::{MailAddress, RawEmail};

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
        let smtp = SmtpSender::new(
            &config.smtp_host,
            config.smtp_port,
            &config.username,
            &config.password,
        );
        let imap = ImapFetcher::new(
            &config.imap_host,
            config.imap_port,
            &config.username,
            &config.password,
        );
        Ok(Self { config, smtp, imap })
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

    /// Validate that the sender address matches the configured maintainer.
    ///
    /// Returns `Err(ChannelError::UnauthorizedSender)` when the addresses differ.
    fn check_sender(&self, raw_from: &str) -> Result<(), ChannelError> {
        let parsed = MailAddress::parse(raw_from).ok_or_else(|| {
            ChannelError::InvalidEnvelope(format!("cannot parse sender address: {raw_from:?}"))
        })?;
        if parsed != self.config.maintainer_address {
            return Err(ChannelError::UnauthorizedSender(format!(
                "message from {parsed} rejected; expected {}",
                self.config.maintainer_address
            )));
        }
        Ok(())
    }

    /// Convert a `RawEmail` to a `ChannelEnvelope`, validating the sender.
    fn to_envelope(&self, email: RawEmail) -> Result<ChannelEnvelope, ChannelError> {
        self.check_sender(&email.from)?;

        let from = format!("email:{}", email.from);
        let to = email
            .to
            .first()
            .map(|a| format!("email:{a}"))
            .unwrap_or_else(|| format!("email:{}", self.config.maintainer_address));

        let mut env = ChannelEnvelope::new(from, to, email.best_body());

        if !email.subject.is_empty() {
            env = env.with_subject(&email.subject);
        }
        if let Some(date) = email.date {
            env = env.with_sent_at(date);
        }
        if let Some(mid) = &email.message_id {
            env = env.with_external_id(mid.as_str());
        }
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

        debug!(from, to, subject, "email send");
        self.smtp
            .send(from, to, subject, &envelope.content, thread_id)
            .await
    }

    async fn poll(&self, since: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
        let raw = self.imap.fetch_since(since, 50).await?;
        let mut envelopes = Vec::new();
        for email in raw {
            match self.to_envelope(email) {
                Ok(env) => envelopes.push(env),
                Err(ChannelError::UnauthorizedSender(msg)) => {
                    warn!("skipping unauthorized message: {msg}");
                }
                Err(e) => return Err(e),
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
            password: "secret".to_string(),
            maintainer_address: MailAddress::parse(maintainer).unwrap(),
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

    fn make_email(from: &str, msg_id: &str) -> RawEmail {
        RawEmail {
            uid: 1,
            message_id: Some(msg_id.to_string()),
            from: from.to_string(),
            to: vec!["me@example.com".to_string()],
            subject: "Hello".to_string(),
            date: None,
            body_text: Some("body text".to_string()),
            body_html: None,
            headers: HashMap::new(),
        }
    }

    fn build_channel(maintainer: &str, emails: Vec<RawEmail>) -> EmailChannel {
        let config = make_config(maintainer);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let smtp = SmtpSender::with_connector(RecordingSmtp {
            calls: calls.clone(),
        });
        let imap = ImapFetcher::with_connector(FixedImap { emails });
        EmailChannel::with_connectors(config, smtp, imap)
    }

    #[test]
    fn kind_is_email() {
        let ch = build_channel("maintainer@example.com", vec![]);
        assert_eq!(ch.kind(), "email");
    }

    #[tokio::test]
    async fn authorized_sender_produces_envelope() {
        let ch = build_channel(
            "maintainer@example.com",
            vec![make_email("maintainer@example.com", "<id1@example.com>")],
        );
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].external_id.as_deref(), Some("<id1@example.com>"));
        assert_eq!(envs[0].from, "email:maintainer@example.com");
    }

    #[tokio::test]
    async fn unauthorized_sender_is_silently_skipped() {
        let ch = build_channel(
            "maintainer@example.com",
            vec![make_email("attacker@example.com", "<evil@example.com>")],
        );
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert!(envs.is_empty(), "unauthorized message must be dropped");
    }

    #[tokio::test]
    async fn display_name_sender_is_normalized() {
        // "Alice Maintainer <maintainer@example.com>" should be accepted.
        let ch = build_channel(
            "maintainer@example.com",
            vec![make_email(
                "Alice Maintainer <maintainer@example.com>",
                "<id2@example.com>",
            )],
        );
        let envs = ch.poll(Utc::now()).await.unwrap();
        assert_eq!(envs.len(), 1, "display-name format must be accepted");
    }

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
