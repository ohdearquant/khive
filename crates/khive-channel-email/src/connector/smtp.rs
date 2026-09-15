//! SMTP outbound connector.
//!
//! Delivers outbound messages via SMTP with mandatory TLS. Credentials are
//! supplied at construction time from environment variables; they are never
//! logged or embedded in source.

use std::{future::Future, sync::Arc, time::Duration};

use async_trait::async_trait;
use khive_channel::ChannelError;
use lettre::{
    message::{header::ContentType, Mailbox},
    transport::smtp::{
        authentication::{Credentials, Mechanism, DEFAULT_MECHANISMS},
        client::{AsyncSmtpConnection, TlsParameters},
        extension::ClientId,
    },
    Message,
};
use tokio::sync::Mutex;
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
///
/// Retains one authenticated connection across sequential deliveries and passes
/// until credentials change or an exchange fails.
pub(crate) struct LettreSmtp {
    host: String,
    port: u16,
    auth: SmtpAuthConfig,
    connection: Mutex<Option<AuthenticatedConnection>>,
}

struct AuthenticatedConnection {
    credentials: Credentials,
    connection: AsyncSmtpConnection,
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
            connection: Mutex::new(None),
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
            connection: Mutex::new(None),
        }
    }

    async fn connect(&self) -> Result<AsyncSmtpConnection, ChannelError> {
        let tls = TlsParameters::new(self.host.clone())
            .map_err(|e| ChannelError::Transport(format!("SMTP relay setup failed: {e}")))?;
        let hello_name = ClientId::default();
        // Match lettre's transport defaults: implicit TLS on 465, mandatory
        // STARTTLS elsewhere, and a 60-second network/command timeout.
        let mut connection = AsyncSmtpConnection::connect_tokio1(
            (self.host.as_str(), self.port),
            Some(Duration::from_secs(60)),
            &hello_name,
            (self.port == 465).then(|| tls.clone()),
            None,
        )
        .await
        .map_err(smtp_connection_error)?;
        if self.port != 465 {
            connection
                .starttls(tls, &hello_name)
                .await
                .map_err(smtp_connection_error)?;
        }
        Ok(connection)
    }

    async fn send_message_with_connect<F, Fut>(
        &self,
        message: Message,
        credentials: Credentials,
        mechanisms: &[Mechanism],
        connect: F,
    ) -> Result<(), ChannelError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<AsyncSmtpConnection, ChannelError>>,
    {
        let mut cached = self.connection.lock().await;
        // Own the connection during every exchange. Cancellation or an error
        // must never return a partially completed SMTP transaction to the cache.
        let mut session = match cached.take() {
            Some(session) if session.credentials == credentials => session,
            _ => {
                let mut connection = connect().await?;
                connection
                    .auth(mechanisms, &credentials)
                    .await
                    .map_err(smtp_connection_error)?;
                classify_smtp_preamble_status(connection.test_connected().await)?;
                AuthenticatedConnection {
                    credentials,
                    connection,
                }
            }
        };

        // Calling the connection directly cannot reconnect inside MAIL/RCPT/DATA.
        // A new handshake always passes through the connection/AUTH classifier.
        session
            .connection
            .send(message.envelope(), &message.formatted())
            .await
            .map_err(|error| {
                classify_smtp_send_error(error.is_permanent(), format!("SMTP send failed: {error}"))
            })?;
        *cached = Some(session);
        Ok(())
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

fn classify_smtp_send_error(is_permanent: bool, message: String) -> ChannelError {
    if is_permanent {
        ChannelError::PermanentTransport(message)
    } else {
        ChannelError::Transport(message)
    }
}

/// Classify a failure from the connect/AUTH preamble (ADR-122 §4's
/// "connection, AUTH" stage grouping), distinct from [`classify_smtp_send_error`]
/// which classifies the post-auth per-message MAIL/RCPT/DATA stage. A
/// permanent (5xx) rejection here applies to the whole account, not one
/// recipient, so it is surfaced as `Auth` rather than a per-message
/// permanent failure: the caller must stop the outbound component for
/// operator action instead of terminally failing the note being sent.
fn classify_smtp_connection_error(is_permanent: bool, message: String) -> ChannelError {
    if is_permanent {
        ChannelError::Auth(message)
    } else {
        ChannelError::Transport(message)
    }
}

fn smtp_connection_error(error: lettre::transport::smtp::Error) -> ChannelError {
    classify_smtp_connection_error(
        error.is_permanent(),
        format!("SMTP connection/authentication failed: {error}"),
    )
}

/// Classify lettre's `test_connected` contract. The connection
/// swallows the NOOP command's own error and reports `false`, so
/// a refused NOOP after a successful connect/AUTH carries no permanence
/// information. AUTH rejections surface as `Err` from the connection setup and
/// are classified by [`classify_smtp_connection_error`]; a refused NOOP is a
/// connection-stage failure of unknown permanence, so it is retried with the
/// outbound backoff and never proceeds to the per-message send.
fn classify_smtp_preamble_status(is_connected: bool) -> Result<(), ChannelError> {
    if is_connected {
        Ok(())
    } else {
        Err(ChannelError::Transport(
            "SMTP connection preamble refused: server did not acknowledge NOOP after connect/AUTH"
                .to_string(),
        ))
    }
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

        let (credentials, mechanisms) = match &self.auth {
            SmtpAuthConfig::Basic(credentials) => (credentials.clone(), DEFAULT_MECHANISMS),
            SmtpAuthConfig::OAuth {
                mailbox,
                token_provider,
            } => {
                let token = token_provider.get_token().await?;
                (
                    Credentials::new(mailbox.clone(), token),
                    &[Mechanism::Xoauth2][..],
                )
            }
        };
        self.send_message_with_connect(msg, credentials, mechanisms, || self.connect())
            .await
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
    use std::{
        collections::VecDeque,
        io,
        net::SocketAddr,
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context, Poll},
    };
    use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};

    #[derive(Debug)]
    struct ScriptedStream(tokio::io::DuplexStream);

    impl lettre::transport::smtp::client::AsyncTokioStream for ScriptedStream {
        fn peer_addr(&self) -> io::Result<SocketAddr> {
            Ok(SocketAddr::from(([127, 0, 0, 1], 25)))
        }
    }

    impl AsyncRead for ScriptedStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for ScriptedStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.0).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }

    type Script = Vec<(String, String)>;

    #[derive(Default)]
    struct SmtpCounts {
        connections: usize,
        auth: usize,
        noop: usize,
        messages: usize,
    }

    struct ScriptedSmtp {
        scripts: Mutex<VecDeque<Script>>,
        counts: Arc<Mutex<SmtpCounts>>,
        servers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
        stalled: Arc<tokio::sync::Notify>,
    }

    impl ScriptedSmtp {
        fn new(scripts: Vec<Script>) -> Self {
            Self {
                scripts: Mutex::new(scripts.into()),
                counts: Arc::new(Mutex::new(SmtpCounts::default())),
                servers: Mutex::new(Vec::new()),
                stalled: Arc::new(tokio::sync::Notify::new()),
            }
        }

        async fn connect(&self) -> Result<AsyncSmtpConnection, ChannelError> {
            let script = self
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected reconnect");
            self.counts.lock().unwrap().connections += 1;
            let counts = self.counts.clone();
            let stalled = self.stalled.clone();
            let (client, server) = tokio::io::duplex(8192);
            let task = tokio::spawn(async move {
                let mut server = BufReader::new(server);
                server.write_all(b"220 scripted SMTP\r\n").await.unwrap();
                for (expected, reply) in script {
                    if expected == "CLOSE" {
                        return;
                    }
                    let mut line = String::new();
                    loop {
                        line.clear();
                        assert_ne!(
                            server.read_line(&mut line).await.unwrap(),
                            0,
                            "missing {expected}"
                        );
                        if expected != "." || line == ".\r\n" {
                            break;
                        }
                    }
                    assert!(
                        line.starts_with(&expected),
                        "expected {expected}, got {line}"
                    );
                    {
                        let mut counts = counts.lock().unwrap();
                        if expected.starts_with("AUTH ") {
                            counts.auth += 1;
                        } else if expected == "NOOP" {
                            counts.noop += 1;
                        } else if expected == "." {
                            counts.messages += 1;
                        }
                    }
                    server.write_all(reply.as_bytes()).await.unwrap();
                    if reply.is_empty() {
                        stalled.notify_one();
                    }
                }
                // Keep the session alive until its owner drops it. Unexpected
                // extra traffic fails instead of silently satisfying a script.
                let mut line = String::new();
                assert_eq!(
                    server.read_line(&mut line).await.unwrap(),
                    0,
                    "extra command: {line}"
                );
            });
            self.servers.lock().unwrap().push(task);
            AsyncSmtpConnection::connect_with_transport(
                Box::new(ScriptedStream(client)),
                &ClientId::default(),
            )
            .await
            .map_err(smtp_connection_error)
        }

        async fn finish(self) -> SmtpCounts {
            assert!(self.scripts.into_inner().unwrap().is_empty());
            for server in self.servers.into_inner().unwrap() {
                tokio::time::timeout(Duration::from_secs(2), server)
                    .await
                    .expect("scripted server did not finish")
                    .expect("scripted server failed");
            }
            Arc::try_unwrap(self.counts)
                .ok()
                .unwrap()
                .into_inner()
                .unwrap()
        }
    }

    fn step(command: &str, response: &str) -> (String, String) {
        (command.to_string(), format!("{response}\r\n"))
    }

    fn handshake(mechanism: Mechanism, secret: &str, auth_reply: &str) -> Script {
        use base64::Engine as _;
        let credentials = Credentials::new("sender@example.com".to_string(), secret.to_string());
        let auth = base64::engine::general_purpose::STANDARD
            .encode(mechanism.response(&credentials, None).unwrap());
        vec![
            step("EHLO ", "250-scripted\r\n250 AUTH PLAIN XOAUTH2"),
            step(&format!("AUTH {mechanism} {auth}\r\n"), auth_reply),
        ]
    }

    fn successful_session(mechanism: Mechanism, secret: &str, messages: usize) -> Script {
        let mut script = handshake(mechanism, secret, "235 authenticated");
        script.push(step("NOOP", "250 ready"));
        for _ in 0..messages {
            script.extend([
                step("MAIL FROM:", "250 sender accepted"),
                step("RCPT TO:", "250 recipient accepted"),
                step("DATA", "354 send message"),
                step(".", "250 queued"),
            ]);
        }
        script
    }

    async fn scripted_delivery(
        connector: &LettreSmtp,
        server: &ScriptedSmtp,
        mechanism: Mechanism,
        secret: &str,
    ) -> Result<(), ChannelError> {
        let message = build_message(
            "sender@example.com",
            "recipient@example.com",
            "subject",
            "body",
            None,
            None,
            None,
            None,
        )
        .unwrap();
        tokio::time::timeout(
            Duration::from_secs(2),
            connector.send_message_with_connect(
                message,
                Credentials::new("sender@example.com".to_string(), secret.to_string()),
                &[mechanism],
                || server.connect(),
            ),
        )
        .await
        .expect("SMTP exchange timed out")
    }

    #[tokio::test]
    async fn smtp_reuses_one_authenticated_connection_for_sequential_deliveries() {
        let connector = LettreSmtp::new("unused.invalid", 587, "unused", "unused");
        let server = ScriptedSmtp::new(vec![successful_session(
            Mechanism::Plain,
            "test-password",
            3,
        )]);
        for _ in 0..3 {
            scripted_delivery(&connector, &server, Mechanism::Plain, "test-password")
                .await
                .unwrap();
        }
        drop(connector);
        let counts = server.finish().await;
        assert_eq!(
            (
                counts.connections,
                counts.auth,
                counts.noop,
                counts.messages
            ),
            (1, 1, 1, 3)
        );
    }

    #[tokio::test]
    async fn smtp_oauth_token_change_reconnects_and_authenticates_with_new_credentials() {
        let connector = LettreSmtp::new("unused.invalid", 587, "unused", "unused");
        let server = ScriptedSmtp::new(vec![
            successful_session(Mechanism::Xoauth2, "test-token-first", 2),
            successful_session(Mechanism::Xoauth2, "test-token-second", 2),
        ]);
        for secret in [
            "test-token-first",
            "test-token-first",
            "test-token-second",
            "test-token-second",
        ] {
            scripted_delivery(&connector, &server, Mechanism::Xoauth2, secret)
                .await
                .unwrap();
        }
        drop(connector);
        let counts = server.finish().await;
        assert_eq!(
            (
                counts.connections,
                counts.auth,
                counts.noop,
                counts.messages
            ),
            (2, 2, 2, 4)
        );
    }

    #[tokio::test]
    async fn smtp_auth_rejection_never_sends_and_failed_session_is_not_cached() {
        let connector = LettreSmtp::new("unused.invalid", 587, "unused", "unused");
        let server = ScriptedSmtp::new(vec![
            handshake(
                Mechanism::Plain,
                "test-password",
                "535 authentication refused",
            ),
            successful_session(Mechanism::Plain, "test-password", 1),
        ]);
        let err = scripted_delivery(&connector, &server, Mechanism::Plain, "test-password")
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::Auth(_)));
        assert_eq!(server.counts.lock().unwrap().messages, 0);
        scripted_delivery(&connector, &server, Mechanism::Plain, "test-password")
            .await
            .unwrap();
        drop(connector);
        let counts = server.finish().await;
        assert_eq!(
            (
                counts.connections,
                counts.auth,
                counts.noop,
                counts.messages
            ),
            (2, 2, 1, 1)
        );
    }

    #[tokio::test]
    async fn smtp_noop_refusal_never_sends_and_next_attempt_reconnects() {
        let connector = LettreSmtp::new("unused.invalid", 587, "unused", "unused");
        let mut refused = handshake(Mechanism::Plain, "test-password", "235 authenticated");
        refused.push(step("NOOP", "550 NOOP refused"));
        let server = ScriptedSmtp::new(vec![
            refused,
            successful_session(Mechanism::Plain, "test-password", 1),
        ]);
        let err = scripted_delivery(&connector, &server, Mechanism::Plain, "test-password")
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::Transport(_)));
        assert_eq!(server.counts.lock().unwrap().messages, 0);
        scripted_delivery(&connector, &server, Mechanism::Plain, "test-password")
            .await
            .unwrap();
        drop(connector);
        let counts = server.finish().await;
        assert_eq!(
            (
                counts.connections,
                counts.auth,
                counts.noop,
                counts.messages
            ),
            (2, 2, 2, 1)
        );
    }

    #[tokio::test]
    async fn smtp_post_auth_rejection_discards_session_and_reconnect_auth_keeps_its_stage() {
        for (reply, permanent) in [
            ("450 recipient busy", false),
            ("550 recipient refused", true),
        ] {
            let connector = LettreSmtp::new("unused.invalid", 587, "unused", "unused");
            let mut refused = successful_session(Mechanism::Plain, "test-password", 1);
            refused.extend([
                step("MAIL FROM:", "250 sender accepted"),
                step("RCPT TO:", reply),
                step("QUIT", "221 goodbye"),
            ]);
            let server = ScriptedSmtp::new(vec![
                refused,
                handshake(
                    Mechanism::Plain,
                    "test-password",
                    "535 authentication refused",
                ),
                successful_session(Mechanism::Plain, "test-password", 1),
            ]);
            scripted_delivery(&connector, &server, Mechanism::Plain, "test-password")
                .await
                .unwrap();
            let err = scripted_delivery(&connector, &server, Mechanism::Plain, "test-password")
                .await
                .unwrap_err();
            assert_eq!(
                matches!(err, ChannelError::PermanentTransport(_)),
                permanent
            );
            if !permanent {
                assert!(matches!(err, ChannelError::Transport(_)));
            }
            let err = scripted_delivery(&connector, &server, Mechanism::Plain, "test-password")
                .await
                .unwrap_err();
            assert!(matches!(err, ChannelError::Auth(_)));
            scripted_delivery(&connector, &server, Mechanism::Plain, "test-password")
                .await
                .unwrap();
            drop(connector);
            let counts = server.finish().await;
            assert_eq!(
                (
                    counts.connections,
                    counts.auth,
                    counts.noop,
                    counts.messages
                ),
                (3, 3, 2, 2)
            );
        }
    }

    #[tokio::test]
    async fn smtp_closed_connection_retries_without_hiding_reconnect_auth_rejection() {
        let connector = LettreSmtp::new("unused.invalid", 587, "unused", "unused");
        let mut closed = successful_session(Mechanism::Plain, "test-password", 1);
        closed.push(("CLOSE".to_string(), String::new()));
        let server = ScriptedSmtp::new(vec![
            closed,
            handshake(
                Mechanism::Plain,
                "test-password",
                "535 authentication refused",
            ),
        ]);
        scripted_delivery(&connector, &server, Mechanism::Plain, "test-password")
            .await
            .unwrap();
        let err = scripted_delivery(&connector, &server, Mechanism::Plain, "test-password")
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::Transport(_)));
        let err = scripted_delivery(&connector, &server, Mechanism::Plain, "test-password")
            .await
            .unwrap_err();
        assert!(matches!(err, ChannelError::Auth(_)));
        drop(connector);
        let counts = server.finish().await;
        assert_eq!(
            (
                counts.connections,
                counts.auth,
                counts.noop,
                counts.messages
            ),
            (2, 2, 1, 1)
        );
    }

    #[tokio::test]
    async fn smtp_cancelled_send_discards_the_in_flight_connection() {
        let connector = Arc::new(LettreSmtp::new("unused.invalid", 587, "unused", "unused"));
        let mut stalled = successful_session(Mechanism::Plain, "test-password", 2);
        stalled.last_mut().unwrap().1.clear();
        let server = Arc::new(ScriptedSmtp::new(vec![
            stalled,
            successful_session(Mechanism::Plain, "test-password", 1),
        ]));
        scripted_delivery(&connector, &server, Mechanism::Plain, "test-password")
            .await
            .unwrap();
        let sender = connector.clone();
        let script = server.clone();
        let delivery = tokio::spawn(async move {
            scripted_delivery(&sender, &script, Mechanism::Plain, "test-password").await
        });
        tokio::time::timeout(Duration::from_secs(2), server.stalled.notified())
            .await
            .unwrap();
        delivery.abort();
        assert!(delivery.await.unwrap_err().is_cancelled());
        scripted_delivery(&connector, &server, Mechanism::Plain, "test-password")
            .await
            .unwrap();
        drop(connector);
        let counts = Arc::try_unwrap(server).ok().unwrap().finish().await;
        assert_eq!(
            (
                counts.connections,
                counts.auth,
                counts.noop,
                counts.messages
            ),
            (2, 2, 2, 3)
        );
    }

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

    #[test]
    fn smtp_reply_classification_keeps_4xx_transient_and_5xx_permanent() {
        let transient = classify_smtp_send_error(false, "450 mailbox unavailable".to_string());
        assert!(matches!(transient, ChannelError::Transport(_)));
        assert_eq!(
            transient.delivery_failure_class(),
            khive_channel::DeliveryFailureClass::Transient
        );

        let permanent = classify_smtp_send_error(true, "550 recipient rejected".to_string());
        assert!(matches!(permanent, ChannelError::PermanentTransport(_)));
        assert_eq!(
            permanent.delivery_failure_class(),
            khive_channel::DeliveryFailureClass::Permanent
        );
    }

    #[test]
    fn smtp_preamble_noop_refusal_is_retried_and_never_reaches_send() {
        assert!(classify_smtp_preamble_status(true).is_ok());
        let refused = classify_smtp_preamble_status(false).unwrap_err();
        assert!(
            matches!(refused, ChannelError::Transport(_)),
            "a refused NOOP carries no permanence information, so it must be a retryable \
             connection-stage failure, got {refused:?}"
        );
        assert!(!matches!(refused, ChannelError::PermanentTransport(_)));
        assert!(!matches!(refused, ChannelError::Auth(_)));
    }

    #[test]
    fn smtp_connection_preamble_permanent_rejection_is_auth_not_permanent_transport() {
        // A definitive rejection during connect/AUTH applies to the whole
        // account, not the one message being sent -- it must not be routed
        // through the same per-message permanent-failure path a rejected
        // recipient uses (classify_smtp_send_error), or the outbox loop
        // would terminally fail one note instead of stopping the component.
        let transient = classify_smtp_connection_error(false, "421 too busy".to_string());
        assert!(matches!(transient, ChannelError::Transport(_)));
        assert_eq!(
            transient.delivery_failure_class(),
            khive_channel::DeliveryFailureClass::Transient
        );

        let permanent =
            classify_smtp_connection_error(true, "535 5.7.8 authentication failed".to_string());
        assert!(matches!(permanent, ChannelError::Auth(_)));
        assert_ne!(
            std::mem::discriminant(&permanent),
            std::mem::discriminant(&ChannelError::PermanentTransport(String::new())),
            "a permanent AUTH rejection must not be classified as a per-message permanent failure"
        );
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
