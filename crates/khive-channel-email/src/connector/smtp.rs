//! SMTP outbound connector.
//!
//! Delivers outbound messages via SMTP with mandatory TLS. Credentials are
//! supplied at construction time from environment variables; they are never
//! logged or embedded in source.

use std::sync::Arc;

use async_trait::async_trait;
use khive_channel::ChannelError;
use lettre::{
    message::{header::ContentType, Mailbox},
    transport::smtp::authentication::{Credentials, Mechanism},
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};
use tracing::instrument;

use crate::oauth::TokenProvider;

/// Custom MIME header for khive thread correlation.
///
/// Attached to outbound messages so that replies can be linked back to the
/// originating thread by the IMAP fetcher.
#[derive(Clone)]
struct XKhiveThreadId(String);

impl lettre::message::header::Header for XKhiveThreadId {
    fn name() -> lettre::message::header::HeaderName {
        lettre::message::header::HeaderName::new_from_ascii_str("X-Khive-Thread-ID")
    }

    fn parse(s: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self(s.trim().to_string()))
    }

    fn display(&self) -> lettre::message::header::HeaderValue {
        lettre::message::header::HeaderValue::new(
            lettre::message::header::HeaderName::new_from_ascii_str("X-Khive-Thread-ID"),
            self.0.clone(),
        )
    }
}

/// SMTP authentication configuration (basic or OAuth2 XOAUTH2).
enum SmtpAuthConfig {
    /// Standard username + password credentials.
    Basic(Credentials),
    /// OAuth2 XOAUTH2: fetch a bearer token from the provider at send time.
    OAuth {
        /// Mailbox address used as the `user=` field in the XOAUTH2 SASL string.
        mailbox: String,
        token_provider: Arc<TokenProvider>,
    },
}

/// Internal trait for the SMTP send operation.
///
/// Allows unit tests to swap in a mock without a live SMTP server.
#[async_trait]
pub(crate) trait SmtpConnector: Send + Sync + 'static {
    async fn deliver(
        &self,
        from: &str,
        to: &str,
        subject: &str,
        body: &str,
        thread_id_header: Option<&str>,
    ) -> Result<(), ChannelError>;
}

/// Production SMTP connector backed by `lettre`.
pub(crate) struct LettreSmtp {
    host: String,
    port: u16,
    auth: SmtpAuthConfig,
}

impl LettreSmtp {
    /// Create a connector using basic username/password credentials.
    pub(crate) fn new(host: impl Into<String>, port: u16, username: &str, password: &str) -> Self {
        Self {
            host: host.into(),
            port,
            auth: SmtpAuthConfig::Basic(Credentials::new(
                username.to_string(),
                password.to_string(),
            )),
        }
    }

    /// Create a connector using XOAUTH2 (Microsoft Exchange Online app-only flow).
    ///
    /// `mailbox` is the address used in the SASL `user=` field.
    /// lettre's `Mechanism::Xoauth2` computes `user=<mailbox>\x01auth=Bearer
    /// <token>\x01\x01` internally from `Credentials::new(mailbox, access_token)`.
    pub(crate) fn new_oauth(
        host: impl Into<String>,
        port: u16,
        mailbox: impl Into<String>,
        token_provider: Arc<TokenProvider>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            auth: SmtpAuthConfig::OAuth {
                mailbox: mailbox.into(),
                token_provider,
            },
        }
    }
}

#[async_trait]
impl SmtpConnector for LettreSmtp {
    #[instrument(skip(self, body), fields(smtp_host = %self.host))]
    async fn deliver(
        &self,
        from: &str,
        to: &str,
        subject: &str,
        body: &str,
        thread_id_header: Option<&str>,
    ) -> Result<(), ChannelError> {
        let from_mb: Mailbox = from.parse().map_err(|e| {
            ChannelError::InvalidEnvelope(format!("invalid from address {from:?}: {e}"))
        })?;
        let to_mb: Mailbox = to.parse().map_err(|e| {
            ChannelError::InvalidEnvelope(format!("invalid to address {to:?}: {e}"))
        })?;

        let mut builder = Message::builder()
            .from(from_mb)
            .to(to_mb)
            .subject(subject)
            .header(ContentType::TEXT_PLAIN);

        if let Some(tid) = thread_id_header {
            builder = builder.header(XKhiveThreadId(tid.to_string()));
        }

        let msg = builder
            .body(body.to_string())
            .map_err(|e| ChannelError::InvalidEnvelope(format!("failed to build message: {e}")))?;

        let relay_builder = AsyncSmtpTransport::<Tokio1Executor>::relay(&self.host)
            .map_err(|e| ChannelError::Transport(format!("SMTP relay setup failed: {e}")))?
            .port(self.port);

        let transport = match &self.auth {
            SmtpAuthConfig::Basic(creds) => relay_builder.credentials(creds.clone()).build(),
            SmtpAuthConfig::OAuth {
                mailbox,
                token_provider,
            } => {
                // Fetch (or return cached) bearer token, then wire it into lettre.
                // lettre's Mechanism::Xoauth2 builds the SASL string internally from
                // Credentials::new(mailbox, access_token).
                let token = token_provider.get_token().await?;
                relay_builder
                    .credentials(Credentials::new(mailbox.clone(), token))
                    .authentication(vec![Mechanism::Xoauth2])
                    .build()
            }
        };

        transport
            .send(msg)
            .await
            .map_err(|e| ChannelError::Transport(format!("SMTP send failed: {e}")))?;

        Ok(())
    }
}

/// SMTP sender wrapping a `SmtpConnector`.
pub struct SmtpSender {
    pub(crate) inner: Arc<dyn SmtpConnector>,
}

impl SmtpSender {
    /// Create a production sender using basic username/password auth.
    pub fn new(host: impl Into<String>, port: u16, username: &str, password: &str) -> Self {
        Self {
            inner: Arc::new(LettreSmtp::new(host, port, username, password)),
        }
    }

    /// Create a production sender using XOAUTH2 (Exchange Online app-only flow).
    pub fn new_oauth(
        host: impl Into<String>,
        port: u16,
        mailbox: impl Into<String>,
        token_provider: Arc<TokenProvider>,
    ) -> Self {
        Self {
            inner: Arc::new(LettreSmtp::new_oauth(host, port, mailbox, token_provider)),
        }
    }

    /// Create a sender wrapping a custom connector (for testing).
    #[cfg(test)]
    pub(crate) fn with_connector(connector: impl SmtpConnector) -> Self {
        Self {
            inner: Arc::new(connector),
        }
    }

    /// Deliver an outbound message. `thread_id` is attached as `X-Khive-Thread-ID`.
    pub async fn send(
        &self,
        from: &str,
        to: &str,
        subject: &str,
        body: &str,
        thread_id: Option<&str>,
    ) -> Result<(), ChannelError> {
        self.inner.deliver(from, to, subject, body, thread_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct MockSmtp {
        calls: Arc<Mutex<Vec<(String, String, String)>>>,
    }

    impl MockSmtp {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl SmtpConnector for MockSmtp {
        async fn deliver(
            &self,
            from: &str,
            to: &str,
            subject: &str,
            _body: &str,
            _thread_id_header: Option<&str>,
        ) -> Result<(), ChannelError> {
            self.calls.lock().unwrap().push((
                from.to_string(),
                to.to_string(),
                subject.to_string(),
            ));
            Ok(())
        }
    }

    #[tokio::test]
    async fn smtp_sender_records_call() {
        let mock = MockSmtp::new();
        let calls = mock.calls.clone();
        let sender = SmtpSender::with_connector(mock);

        sender
            .send(
                "from@example.com",
                "to@example.com",
                "Hello",
                "body text",
                None,
            )
            .await
            .expect("send ok");

        let locked = calls.lock().unwrap();
        assert_eq!(locked.len(), 1);
        assert_eq!(locked[0].0, "from@example.com");
        assert_eq!(locked[0].1, "to@example.com");
        assert_eq!(locked[0].2, "Hello");
    }

    #[tokio::test]
    async fn smtp_sender_passes_thread_id() {
        struct CapturingSmtp {
            headers: Arc<Mutex<Vec<Option<String>>>>,
        }

        #[async_trait]
        impl SmtpConnector for CapturingSmtp {
            async fn deliver(
                &self,
                _from: &str,
                _to: &str,
                _subject: &str,
                _body: &str,
                thread_id_header: Option<&str>,
            ) -> Result<(), ChannelError> {
                self.headers
                    .lock()
                    .unwrap()
                    .push(thread_id_header.map(|s| s.to_string()));
                Ok(())
            }
        }

        let headers = Arc::new(Mutex::new(Vec::new()));
        let sender = SmtpSender::with_connector(CapturingSmtp {
            headers: headers.clone(),
        });

        sender
            .send("a@example.com", "b@example.com", "s", "b", Some("tid-abc"))
            .await
            .unwrap();

        let captured = headers.lock().unwrap();
        assert_eq!(captured[0].as_deref(), Some("tid-abc"));
    }
}
