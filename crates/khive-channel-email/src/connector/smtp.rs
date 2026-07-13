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
    #[allow(clippy::too_many_arguments)]
    async fn deliver(
        &self,
        from: &str,
        to: &str,
        subject: &str,
        body: &str,
        thread_id_header: Option<&str>,
        message_id: Option<&str>,
        in_reply_to: Option<&str>,
        references: Option<&str>,
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

/// Reject a header value carrying CR or LF (header/line injection guard).
///
/// `In-Reply-To`/`References` values reach this module already assembled by
/// the caller (khive-pack-comm sanitizes and wraps each token). This is the
/// last defensive check before a value reaches a `lettre` header setter, none
/// of which validate their input (they store the raw string verbatim).
fn reject_crlf(value: &str) -> Result<&str, ChannelError> {
    if value.contains(['\r', '\n']) {
        return Err(ChannelError::InvalidEnvelope(
            "header value must not contain CR or LF".to_string(),
        ));
    }
    Ok(value)
}

/// Build the outbound RFC 822 message, applying thread-correlation, Message-ID, and
/// reply-threading headers.
///
/// Pure (no I/O) so unit tests can assert on the actual serialized header bytes via
/// `Message::formatted()` without a live transport.
#[allow(clippy::too_many_arguments)]
fn build_message(
    from: &str,
    to: &str,
    subject: &str,
    body: &str,
    thread_id_header: Option<&str>,
    message_id: Option<&str>,
    in_reply_to: Option<&str>,
    references: Option<&str>,
) -> Result<Message, ChannelError> {
    let from_mb: Mailbox = from.parse().map_err(|e| {
        ChannelError::InvalidEnvelope(format!("invalid from address {from:?}: {e}"))
    })?;
    let to_mb: Mailbox = to
        .parse()
        .map_err(|e| ChannelError::InvalidEnvelope(format!("invalid to address {to:?}: {e}")))?;

    let mut builder = Message::builder()
        .from(from_mb)
        .to(to_mb)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN);

    if let Some(tid) = thread_id_header {
        builder = builder.header(XKhiveThreadId(tid.to_string()));
    }

    if let Some(mid) = message_id {
        builder = builder.message_id(Some(mid.to_string()));
    }

    // In-Reply-To/References drive native MUA conversation grouping (issue #403).
    // khive's own thread continuity uses X-Khive-Thread-ID/external_id instead, so
    // these are set only when a parent wire Message-ID is known -- no error, no
    // placeholder, when it is not. In-Reply-To is always exactly the parent
    // Message-ID. References is the full ancestor chain assembled by the caller
    // (khive-pack-comm's `build_references_header`); when no chain was computed
    // (e.g. a parent whose own chain is unknown), it degrades gracefully to the
    // parent Message-ID alone -- identical to pre-chain-preservation behavior.
    if let Some(irt) = in_reply_to {
        builder = builder.in_reply_to(reject_crlf(irt)?.to_string());
        let refs = references.unwrap_or(irt);
        builder = builder.references(reject_crlf(refs)?.to_string());
    }

    builder
        .body(body.to_string())
        .map_err(|e| ChannelError::InvalidEnvelope(format!("failed to build message: {e}")))
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
        message_id: Option<&str>,
        in_reply_to: Option<&str>,
        references: Option<&str>,
    ) -> Result<(), ChannelError> {
        let msg = build_message(
            from,
            to,
            subject,
            body,
            thread_id_header,
            message_id,
            in_reply_to,
            references,
        )?;

        // Port 465 is implicit TLS (SMTPS, TLS-on-connect); 587 and everything
        // else use STARTTLS (connect in plaintext, upgrade after EHLO). Exchange
        // Online's SMTP AUTH submission endpoint is 587/STARTTLS. Using implicit
        // TLS on a STARTTLS port makes rustls read the plaintext `220` greeting as
        // a TLS record and fail with `InvalidContentType`.
        let relay_builder = if self.port == 465 {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&self.host)
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.host)
        }
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

    /// Deliver an outbound message.
    ///
    /// `thread_id` is attached as `X-Khive-Thread-ID`. `message_id` is set as the
    /// RFC 822 `Message-ID` header verbatim (caller must include angle brackets);
    /// pass `None` to let lettre auto-generate. `in_reply_to`, when present, is set
    /// as `In-Reply-To` verbatim (caller must include angle brackets) for native
    /// MUA conversation grouping; pass `None` when the reply has no known parent
    /// Message-ID. `references` is the full ancestor chain to set as `References`
    /// (space-separated angle-bracketed ids); when `in_reply_to` is present but
    /// `references` is `None`, `References` falls back to `in_reply_to` alone.
    #[allow(clippy::too_many_arguments)]
    pub async fn send(
        &self,
        from: &str,
        to: &str,
        subject: &str,
        body: &str,
        thread_id: Option<&str>,
        message_id: Option<&str>,
        in_reply_to: Option<&str>,
        references: Option<&str>,
    ) -> Result<(), ChannelError> {
        self.inner
            .deliver(
                from,
                to,
                subject,
                body,
                thread_id,
                message_id,
                in_reply_to,
                references,
            )
            .await
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
            _message_id: Option<&str>,
            _in_reply_to: Option<&str>,
            _references: Option<&str>,
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
                None,
                None,
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
                _message_id: Option<&str>,
                _in_reply_to: Option<&str>,
                _references: Option<&str>,
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
            .send(
                "a@example.com",
                "b@example.com",
                "s",
                "b",
                Some("tid-abc"),
                None,
                None,
                None,
            )
            .await
            .unwrap();

        let captured = headers.lock().unwrap();
        assert_eq!(captured[0].as_deref(), Some("tid-abc"));
    }

    #[tokio::test]
    async fn smtp_sender_passes_message_id() {
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
                _thread_id_header: Option<&str>,
                message_id: Option<&str>,
                _in_reply_to: Option<&str>,
                _references: Option<&str>,
            ) -> Result<(), ChannelError> {
                self.captured
                    .lock()
                    .unwrap()
                    .push(message_id.map(|s| s.to_string()));
                Ok(())
            }
        }

        let captured = Arc::new(Mutex::new(Vec::new()));
        let sender = SmtpSender::with_connector(CapturingSmtp {
            captured: captured.clone(),
        });

        sender
            .send(
                "a@example.com",
                "b@example.com",
                "s",
                "b",
                None,
                Some("<abc123@example.com>"),
                None,
                None,
            )
            .await
            .unwrap();

        let vals = captured.lock().unwrap();
        assert_eq!(vals[0].as_deref(), Some("<abc123@example.com>"));
    }

    #[tokio::test]
    async fn smtp_sender_passes_in_reply_to() {
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
                _thread_id_header: Option<&str>,
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

        let captured = Arc::new(Mutex::new(Vec::new()));
        let sender = SmtpSender::with_connector(CapturingSmtp {
            captured: captured.clone(),
        });

        sender
            .send(
                "a@example.com",
                "b@example.com",
                "s",
                "b",
                None,
                None,
                Some("<parent123@example.com>"),
                None,
            )
            .await
            .unwrap();

        let vals = captured.lock().unwrap();
        assert_eq!(vals[0].as_deref(), Some("<parent123@example.com>"));
    }

    #[tokio::test]
    async fn smtp_sender_passes_references() {
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
                _thread_id_header: Option<&str>,
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

        let captured = Arc::new(Mutex::new(Vec::new()));
        let sender = SmtpSender::with_connector(CapturingSmtp {
            captured: captured.clone(),
        });

        sender
            .send(
                "a@example.com",
                "b@example.com",
                "s",
                "b",
                None,
                None,
                Some("<parent123@example.com>"),
                Some("<grandparent1@example.com> <parent123@example.com>"),
            )
            .await
            .unwrap();

        let vals = captured.lock().unwrap();
        assert_eq!(
            vals[0].as_deref(),
            Some("<grandparent1@example.com> <parent123@example.com>")
        );
    }

    // --- build_message: real RFC 822 header assembly (issue #403) ---

    fn formatted_str(msg: &Message) -> String {
        String::from_utf8(msg.formatted()).expect("formatted message is valid UTF-8")
    }

    /// Undo RFC 5322 §2.2.3 header folding (`lettre` wraps long header lines by
    /// inserting `\r\n` followed by whitespace) so long-value assertions (e.g. a
    /// multi-id References chain) can match the logical header value rather than
    /// the wire-wrapped bytes. A real MUA unfolds identically before parsing.
    fn unfold(s: &str) -> String {
        s.replace("\r\n ", " ").replace("\r\n\t", " ")
    }

    #[test]
    fn build_message_sets_in_reply_to_and_references() {
        // No explicit chain supplied: References falls back to the parent
        // Message-ID alone, identical to pre-chain-preservation behavior.
        let msg = build_message(
            "a@example.com",
            "b@example.com",
            "subject",
            "body",
            None,
            None,
            Some("<parent123@example.com>"),
            None,
        )
        .expect("build_message ok");

        let formatted = formatted_str(&msg);
        assert!(
            formatted.contains("In-Reply-To: <parent123@example.com>"),
            "formatted message must carry In-Reply-To; got:\n{formatted}"
        );
        assert!(
            formatted.contains("References: <parent123@example.com>"),
            "formatted message must carry References; got:\n{formatted}"
        );
    }

    #[test]
    fn build_message_omits_in_reply_to_when_absent() {
        let msg = build_message(
            "a@example.com",
            "b@example.com",
            "subject",
            "body",
            None,
            None,
            None,
            None,
        )
        .expect("build_message ok");

        let formatted = formatted_str(&msg);
        assert!(
            !formatted.contains("In-Reply-To:"),
            "no parent Message-ID must mean no In-Reply-To header; got:\n{formatted}"
        );
        assert!(
            !formatted.contains("References:"),
            "no parent Message-ID must mean no References header; got:\n{formatted}"
        );
    }

    #[test]
    fn build_message_sets_message_id_and_thread_header_together_with_in_reply_to() {
        // Regression guard: In-Reply-To must not clobber the other optional headers
        // when all three are present on the same outbound reply.
        let msg = build_message(
            "a@example.com",
            "b@example.com",
            "subject",
            "body",
            Some("thread-xyz"),
            Some("<self123@example.com>"),
            Some("<parent123@example.com>"),
            None,
        )
        .expect("build_message ok");

        let formatted = formatted_str(&msg);
        assert!(formatted.contains("X-Khive-Thread-ID: thread-xyz"));
        assert!(formatted.contains("Message-ID: <self123@example.com>"));
        assert!(formatted.contains("In-Reply-To: <parent123@example.com>"));
        assert!(formatted.contains("References: <parent123@example.com>"));
    }

    #[test]
    fn build_message_references_carries_full_ancestor_chain() {
        // Issue #403: References must be the parent's existing chain
        // (2+ ids here) followed by the parent's own Message-ID -- NOT just the
        // immediate parent. In-Reply-To stays the parent Message-ID only.
        let msg = build_message(
            "a@example.com",
            "b@example.com",
            "subject",
            "body",
            None,
            None,
            Some("<parent123@example.com>"),
            Some("<grandparent1@example.com> <grandparent2@example.com> <parent123@example.com>"),
        )
        .expect("build_message ok");

        let formatted = unfold(&formatted_str(&msg));
        assert!(
            formatted.contains("In-Reply-To: <parent123@example.com>"),
            "In-Reply-To must be exactly the parent Message-ID; got:\n{formatted}"
        );
        assert!(
            formatted.contains(
                "References: <grandparent1@example.com> <grandparent2@example.com> <parent123@example.com>"
            ),
            "References must carry the full ancestor chain, not just the immediate parent; got:\n{formatted}"
        );
    }

    #[test]
    fn build_message_references_falls_back_to_in_reply_to_when_chain_absent() {
        // A parent with a known Message-ID but no References chain of its own
        // (e.g. it was itself a thread root): References degrades gracefully to
        // the parent Message-ID alone, matching the pre-chain-preservation shape.
        let msg = build_message(
            "a@example.com",
            "b@example.com",
            "subject",
            "body",
            None,
            None,
            Some("<parent123@example.com>"),
            None,
        )
        .expect("build_message ok");

        let formatted = formatted_str(&msg);
        assert!(formatted.contains("In-Reply-To: <parent123@example.com>"));
        assert!(formatted.contains("References: <parent123@example.com>"));
        assert!(
            !formatted.contains("References: <parent123@example.com> <parent123@example.com>"),
            "must not duplicate the parent id when no chain is supplied; got:\n{formatted}"
        );
    }

    #[test]
    fn build_message_rejects_crlf_in_references() {
        let err = build_message(
            "a@example.com",
            "b@example.com",
            "subject",
            "body",
            None,
            None,
            Some("<parent123@example.com>"),
            Some("<evil@example.com>\r\nBcc: attacker@evil.com"),
        )
        .expect_err("CRLF in References must be rejected, not silently forwarded");

        assert!(matches!(err, ChannelError::InvalidEnvelope(_)));
    }
}
