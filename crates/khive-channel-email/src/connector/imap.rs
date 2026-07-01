//! IMAP inbound connector.
//!
//! Fetches new messages from the INBOX since a given timestamp. TLS is always
//! required; plaintext IMAP connections are rejected. Credentials are supplied
//! at construction time from environment variables.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use khive_channel::ChannelError;
use mail_parser::MessageParser;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::instrument;

use crate::oauth::{TokenProvider, XOAuth2Authenticator};

use super::RawEmail;

/// IMAP session type using TLS over a compat-wrapped tokio stream.
type ImapSession = async_imap::Session<
    async_native_tls::TlsStream<tokio_util::compat::Compat<tokio::net::TcpStream>>,
>;

/// Internal trait for IMAP fetch operations.
///
/// Allows unit tests to substitute a mock without a live IMAP server.
#[async_trait]
pub(crate) trait ImapConnector: Send + Sync + 'static {
    async fn fetch_since(
        &self,
        since: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<RawEmail>, ChannelError>;
}

/// IMAP authentication configuration (basic or OAuth2 XOAUTH2).
enum ImapAuthConfig {
    /// Standard IMAP LOGIN with username and password.
    Basic { username: String, password: String },
    /// IMAP AUTHENTICATE XOAUTH2 with a bearer token fetched from the provider.
    OAuth {
        /// Mailbox address used as the `user=` field in the XOAUTH2 SASL string.
        mailbox: String,
        token_provider: Arc<TokenProvider>,
    },
}

/// Production IMAP connector backed by `async-imap` with native TLS.
pub(crate) struct LiveImap {
    host: String,
    port: u16,
    auth: ImapAuthConfig,
}

impl LiveImap {
    /// Create a connector using basic IMAP LOGIN credentials.
    pub(crate) fn new(
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            auth: ImapAuthConfig::Basic {
                username: username.into(),
                password: password.into(),
            },
        }
    }

    /// Create a connector using XOAUTH2 (Exchange Online app-only flow).
    ///
    /// `mailbox` is the address used in the SASL `user=` field.
    /// async-imap 0.9's `Client::authenticate("XOAUTH2", authenticator)` is used;
    /// the authenticator's `process()` returns the raw SASL bytes which async-imap
    /// base64-encodes before sending.
    pub(crate) fn new_oauth(
        host: impl Into<String>,
        port: u16,
        mailbox: impl Into<String>,
        token_provider: Arc<TokenProvider>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            auth: ImapAuthConfig::OAuth {
                mailbox: mailbox.into(),
                token_provider,
            },
        }
    }

    async fn connect(&self) -> Result<ImapSession, ChannelError> {
        // TCP connect with 10s timeout.
        let tcp = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::net::TcpStream::connect((self.host.as_str(), self.port)),
        )
        .await
        .map_err(|_| ChannelError::Transport("IMAP TCP connect timed out (10s)".into()))?
        .map_err(|e| ChannelError::Transport(format!("IMAP TCP connect failed: {e}")))?;

        // Wrap with compat layer so async-native-tls (futures-io) can use the stream.
        let tcp_compat = tcp.compat();

        // TLS handshake with 15s timeout.
        let tls_connector = async_native_tls::TlsConnector::new();
        let tls_stream = tokio::time::timeout(
            Duration::from_secs(15),
            tls_connector.connect(&self.host, tcp_compat),
        )
        .await
        .map_err(|_| ChannelError::Auth("IMAP TLS handshake timed out (15s)".into()))?
        .map_err(|e| ChannelError::Auth(format!("IMAP TLS handshake failed: {e}")))?;

        let mut client = async_imap::Client::new(tls_stream);

        // Consume the server greeting before issuing any command. `Client::new`
        // (unlike `async_imap::connect`) does not read it, and an unconsumed
        // greeting desyncs the response parser: the first command's reply reads
        // the `* OK ...` greeting instead of its own tagged response, then blocks
        // waiting for a reply that never arrives (surfaces as the auth timeout).
        // Direct-TLS on 993 always sends a greeting (unlike STARTTLS on 143).
        match tokio::time::timeout(Duration::from_secs(15), client.read_response())
            .await
            .map_err(|_| ChannelError::Transport("IMAP greeting read timed out (15s)".into()))?
        {
            Some(Ok(_)) => {}
            Some(Err(e)) => {
                return Err(ChannelError::Transport(format!(
                    "IMAP greeting read failed: {e}"
                )));
            }
            None => {
                return Err(ChannelError::Transport(
                    "IMAP connection closed before greeting".into(),
                ));
            }
        }

        // Authenticate with 15s timeout, dispatching on auth mode.
        let session: ImapSession = match &self.auth {
            ImapAuthConfig::Basic { username, password } => {
                tokio::time::timeout(Duration::from_secs(15), client.login(username, password))
                    .await
                    .map_err(|_| ChannelError::Auth("IMAP login timed out (15s)".into()))?
                    .map_err(|(e, _)| ChannelError::Auth(format!("IMAP login failed: {e}")))?
            }
            ImapAuthConfig::OAuth {
                mailbox,
                token_provider,
            } => {
                // Fetch (or return cached) bearer token before the IMAP handshake.
                // XOAuth2Authenticator::process returns the raw SASL bytes;
                // async-imap 0.9 base64-encodes them in do_auth_handshake (client.rs:282).
                let token = token_provider.get_token().await?;
                let authenticator = XOAuth2Authenticator {
                    mailbox: mailbox.clone(),
                    token,
                };
                tokio::time::timeout(
                    Duration::from_secs(15),
                    client.authenticate("XOAUTH2", authenticator),
                )
                .await
                .map_err(|_| {
                    ChannelError::Auth("IMAP XOAUTH2 authenticate timed out (15s)".into())
                })?
                .map_err(|(e, _)| {
                    ChannelError::Auth(format!("IMAP XOAUTH2 authenticate failed: {e}"))
                })?
            }
        };

        Ok(session)
    }
}

#[async_trait]
impl ImapConnector for LiveImap {
    #[instrument(skip(self), fields(imap_host = %self.host))]
    async fn fetch_since(
        &self,
        since: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<RawEmail>, ChannelError> {
        let mut session = self.connect().await?;

        // SELECT returns the Mailbox struct; uid_validity is the UIDVALIDITY value needed
        // for stable per-message dedup keys of the form `imap:{host}:{uidvalidity}:{uid}`.
        let mailbox = session
            .select("INBOX")
            .await
            .map_err(|e| ChannelError::Transport(format!("IMAP SELECT INBOX failed: {e}")))?;
        let uid_validity = mailbox.uid_validity;

        // Search for messages since the given date.
        let since_str = since.format("%d-%b-%Y").to_string();
        let uid_set = session
            .uid_search(format!("SINCE {since_str}"))
            .await
            .map_err(|e| ChannelError::Transport(format!("IMAP UID SEARCH SINCE failed: {e}")))?;

        let mut uid_list: Vec<u32> = uid_set.into_iter().collect();
        uid_list.truncate(limit);

        if uid_list.is_empty() {
            let _ = session.logout().await;
            return Ok(vec![]);
        }

        let uid_str = uid_list
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        // Collect the fetch stream into owned bytes before releasing the session borrow.
        // Each entry carries the raw UID as returned by the server (Option<u32>); zero
        // and absent UIDs are validated inside process_mailbox_fetch.
        let fetched_raw: Vec<(Option<u32>, Vec<u8>)> = {
            let mut stream = session
                .uid_fetch(&uid_str, "RFC822")
                .await
                .map_err(|e| ChannelError::Transport(format!("IMAP UID FETCH failed: {e}")))?;

            let mut collected = Vec::new();
            while let Some(msg) = stream
                .try_next()
                .await
                .map_err(|e| ChannelError::Transport(format!("IMAP fetch stream error: {e}")))?
            {
                if let Some(body) = msg.body() {
                    collected.push((msg.uid, body.to_vec()));
                }
            }
            collected
        };

        let _ = session.logout().await;

        process_mailbox_fetch(uid_validity, fetched_raw, &self.host)
    }
}

/// Validate UIDVALIDITY + per-message UIDs and build the `RawEmail` list.
///
/// `uid_validity` must be `Some(non-zero)`.  When it is `None` or `Some(0)` the
/// whole batch is rejected as a transport error: UIDVALIDITY is required to form
/// the stable `imap:{host}:{uidvalidity}:{uid}` dedup key, and without it we
/// cannot safely identify or deduplicate messages.
///
/// Per-message UIDs that are `None` or `0` are skipped with a `warn!` log rather
/// than failing the batch: one missing UID is a server quirk, not a reason to
/// discard all other messages in the poll.
///
/// This function is extracted from `LiveImap::fetch_since` so the validation
/// logic can be exercised without a live IMAP server.
pub(crate) fn process_mailbox_fetch(
    uid_validity: Option<u32>,
    fetched_raw: Vec<(Option<u32>, Vec<u8>)>,
    host: &str,
) -> Result<Vec<RawEmail>, ChannelError> {
    let uidvalidity = match uid_validity {
        Some(v) if v != 0 => v,
        _ => {
            return Err(ChannelError::Transport(
                "IMAP SELECT did not return a valid UIDVALIDITY; \
                 cannot safely deduplicate messages — poll aborted"
                    .to_string(),
            ));
        }
    };

    let mut result = Vec::new();
    for (uid_opt, raw_bytes) in fetched_raw {
        let uid = match uid_opt {
            Some(u) if u != 0 => u,
            _ => {
                tracing::warn!(
                    host = %host,
                    uidvalidity = %uidvalidity,
                    "skipping message with missing or zero UID; cannot form a stable dedup key"
                );
                continue;
            }
        };
        if let Some(email) = parse_raw_bytes(uid, &raw_bytes, host, uidvalidity) {
            result.push(email);
        }
    }
    Ok(result)
}

/// Parse raw RFC 822 bytes into a `RawEmail`.
///
/// `host` and `uidvalidity` are combined with `uid` to form the stable
/// `imap_external_id` dedup key. This avoids relying on the `Message-ID`
/// header, which is optional and could be absent or spoofed.
pub(crate) fn parse_raw_bytes(
    uid: u32,
    raw: &[u8],
    host: &str,
    uidvalidity: u32,
) -> Option<RawEmail> {
    let parser = MessageParser::default();
    let msg = parser.parse(raw)?;

    // Stable dedup key: imap:{host}:{uidvalidity}:{uid}.
    // Always set; never depends on Message-ID.
    let imap_external_id = format!("imap:{host}:{uidvalidity}:{uid}");

    // Collect all From addresses as addr-specs (display names stripped by mail_parser).
    let from_addrs: Vec<String> = msg
        .from()
        .map(|addrs| {
            addrs
                .iter()
                .filter_map(|a| a.address())
                .map(|s| s.to_lowercase())
                .collect()
        })
        .unwrap_or_default();

    // Sender header (RFC 5322: single mailbox; first entry is the canonical one).
    let sender_addr: Option<String> = msg
        .sender()
        .and_then(|a| a.first())
        .and_then(|a| a.address())
        .map(|s| s.to_lowercase());

    let to: Vec<String> = msg
        .to()
        .map(|addrs| {
            addrs
                .iter()
                .filter_map(|a| a.address())
                .map(|s| s.to_lowercase())
                .collect()
        })
        .unwrap_or_default();

    let subject = msg.subject().map(|s| s.to_string()).unwrap_or_default();

    let date = msg
        .date()
        .and_then(|d| DateTime::from_timestamp(d.to_timestamp(), 0));

    let body_text = msg.body_text(0).map(|s| s.to_string());

    let body_html = msg.body_html(0).map(|s| s.to_string());

    // Collect headers into a flat lowercase map (first occurrence wins).
    let mut headers: HashMap<String, String> = HashMap::new();
    for header in msg.headers() {
        let key = header.name().to_lowercase();
        if let std::collections::hash_map::Entry::Vacant(e) = headers.entry(key) {
            if let mail_parser::HeaderValue::Text(v) = header.value() {
                e.insert(v.to_string());
            }
        }
    }

    Some(RawEmail {
        uid,
        imap_external_id,
        from_addrs,
        sender_addr,
        to,
        subject,
        date,
        body_text,
        body_html,
        headers,
    })
}

/// IMAP fetcher wrapping an `ImapConnector`.
pub struct ImapFetcher {
    pub(crate) inner: Arc<dyn ImapConnector>,
}

impl ImapFetcher {
    /// Create a production fetcher using basic IMAP LOGIN credentials.
    pub fn new(host: impl Into<String>, port: u16, username: &str, password: &str) -> Self {
        Self {
            inner: Arc::new(LiveImap::new(host, port, username, password)),
        }
    }

    /// Create a production fetcher using XOAUTH2 (Exchange Online app-only flow).
    pub fn new_oauth(
        host: impl Into<String>,
        port: u16,
        mailbox: impl Into<String>,
        token_provider: Arc<TokenProvider>,
    ) -> Self {
        Self {
            inner: Arc::new(LiveImap::new_oauth(host, port, mailbox, token_provider)),
        }
    }

    /// Create a fetcher wrapping a custom connector (for testing).
    #[cfg(test)]
    pub(crate) fn with_connector(connector: impl ImapConnector) -> Self {
        Self {
            inner: Arc::new(connector),
        }
    }

    /// Fetch messages received since `since`, up to `limit` items.
    pub async fn fetch_since(
        &self,
        since: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<RawEmail>, ChannelError> {
        self.inner.fetch_since(since, limit).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockImap {
        emails: Vec<RawEmail>,
    }

    impl MockImap {
        fn with_emails(emails: Vec<RawEmail>) -> Self {
            Self { emails }
        }
    }

    #[async_trait]
    impl ImapConnector for MockImap {
        async fn fetch_since(
            &self,
            _since: DateTime<Utc>,
            _limit: usize,
        ) -> Result<Vec<RawEmail>, ChannelError> {
            Ok(self.emails.clone())
        }
    }

    fn make_email(uid: u32, imap_id: &str, from_addr: &str) -> RawEmail {
        RawEmail {
            uid,
            imap_external_id: imap_id.to_string(),
            from_addrs: vec![from_addr.to_string()],
            sender_addr: None,
            to: vec!["me@example.com".to_string()],
            subject: "Test".to_string(),
            date: None,
            body_text: Some("body".to_string()),
            body_html: None,
            headers: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn mock_imap_returns_emails() {
        let emails = vec![make_email(
            1,
            "imap:mail.example.com:12345:1",
            "alice@example.com",
        )];
        let fetcher = ImapFetcher::with_connector(MockImap::with_emails(emails));
        let result = fetcher.fetch_since(Utc::now(), 50).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].imap_external_id, "imap:mail.example.com:12345:1");
        assert_eq!(result[0].from_addrs, vec!["alice@example.com"]);
    }

    #[tokio::test]
    async fn mock_imap_returns_empty_when_no_messages() {
        let fetcher = ImapFetcher::with_connector(MockImap::with_emails(vec![]));
        let result = fetcher.fetch_since(Utc::now(), 50).await.unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn parse_raw_bytes_extracts_all_from_addresses() {
        // RFC 5322 allows multiple addresses in From.
        let raw = b"From: alice@example.com, bob@example.com\r\n\
                    To: me@example.com\r\n\
                    Subject: Multi-From test\r\n\
                    \r\n\
                    body";
        let email = parse_raw_bytes(42, raw, "imap.example.com", 9999).unwrap();
        assert_eq!(email.imap_external_id, "imap:imap.example.com:9999:42");
        assert_eq!(email.from_addrs.len(), 2);
        assert!(email.from_addrs.contains(&"alice@example.com".to_string()));
        assert!(email.from_addrs.contains(&"bob@example.com".to_string()));
    }

    #[test]
    fn parse_raw_bytes_extracts_sender_header() {
        let raw = b"From: alice@example.com\r\n\
                    Sender: sender@example.com\r\n\
                    To: me@example.com\r\n\
                    Subject: Sender test\r\n\
                    \r\n\
                    body";
        let email = parse_raw_bytes(7, raw, "imap.example.com", 1).unwrap();
        assert_eq!(email.sender_addr.as_deref(), Some("sender@example.com"));
    }

    #[test]
    fn parse_raw_bytes_stable_id_without_message_id() {
        // Message has no Message-ID header; imap_external_id must still be set.
        let raw = b"From: alice@example.com\r\n\
                    To: me@example.com\r\n\
                    Subject: No Message-ID\r\n\
                    \r\n\
                    body";
        let email = parse_raw_bytes(3, raw, "imap.example.com", 5555).unwrap();
        assert_eq!(email.imap_external_id, "imap:imap.example.com:5555:3");
        assert!(!email.imap_external_id.is_empty());
    }

    #[test]
    fn raw_email_khive_thread_id_header() {
        let mut headers = HashMap::new();
        headers.insert("x-khive-thread-id".to_string(), "some-uuid".to_string());
        let email = RawEmail {
            uid: 1,
            imap_external_id: "imap:host:1:1".to_string(),
            from_addrs: vec!["a@example.com".to_string()],
            sender_addr: None,
            to: vec![],
            subject: String::new(),
            date: None,
            body_text: None,
            body_html: None,
            headers,
        };
        assert_eq!(email.khive_thread_id(), Some("some-uuid"));
        assert_eq!(email.correlation(), Some("some-uuid"));
    }

    // --- process_mailbox_fetch guard tests ---

    fn minimal_rfc822(from: &str, subject: &str) -> Vec<u8> {
        format!("From: {from}\r\nTo: me@example.com\r\nSubject: {subject}\r\n\r\nbody").into_bytes()
    }

    #[test]
    fn process_mailbox_fetch_missing_uidvalidity_returns_error() {
        let raw = minimal_rfc822("a@example.com", "test");
        let result = process_mailbox_fetch(None, vec![(Some(1), raw)], "imap.example.com");
        assert!(
            result.is_err(),
            "missing UIDVALIDITY must return a transport error"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("UIDVALIDITY"),
            "error message should mention UIDVALIDITY; got: {msg}"
        );
    }

    #[test]
    fn process_mailbox_fetch_zero_uidvalidity_returns_error() {
        let raw = minimal_rfc822("a@example.com", "test");
        let result = process_mailbox_fetch(Some(0), vec![(Some(1), raw)], "imap.example.com");
        assert!(
            result.is_err(),
            "zero UIDVALIDITY must return a transport error"
        );
    }

    #[test]
    fn process_mailbox_fetch_missing_uid_skips_message() {
        let raw = minimal_rfc822("a@example.com", "test");
        let result =
            process_mailbox_fetch(Some(999), vec![(None, raw)], "imap.example.com").unwrap();
        assert!(
            result.is_empty(),
            "a message with missing UID must be skipped (not an error)"
        );
    }

    #[test]
    fn process_mailbox_fetch_zero_uid_skips_message() {
        let raw = minimal_rfc822("a@example.com", "test");
        let result =
            process_mailbox_fetch(Some(999), vec![(Some(0), raw)], "imap.example.com").unwrap();
        assert!(
            result.is_empty(),
            "a message with zero UID must be skipped (not an error)"
        );
    }

    #[test]
    fn process_mailbox_fetch_valid_inputs_produce_email() {
        let raw = minimal_rfc822("alice@example.com", "hello");
        let result =
            process_mailbox_fetch(Some(1234), vec![(Some(7), raw)], "mail.example.com").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].imap_external_id, "imap:mail.example.com:1234:7");
    }

    #[test]
    fn process_mailbox_fetch_skips_invalid_uid_continues_valid() {
        let raw_bad = minimal_rfc822("bad@example.com", "bad");
        let raw_good = minimal_rfc822("good@example.com", "good");
        let result = process_mailbox_fetch(
            Some(555),
            vec![(Some(0), raw_bad), (Some(3), raw_good)],
            "imap.example.com",
        )
        .unwrap();
        assert_eq!(result.len(), 1, "only the valid message should be returned");
        assert_eq!(result[0].imap_external_id, "imap:imap.example.com:555:3");
    }

    #[test]
    fn raw_email_in_reply_to_fallback() {
        let mut headers = HashMap::new();
        headers.insert("in-reply-to".to_string(), "<orig@example.com>".to_string());
        let email = RawEmail {
            uid: 2,
            imap_external_id: "imap:host:1:2".to_string(),
            from_addrs: vec!["b@example.com".to_string()],
            sender_addr: None,
            to: vec![],
            subject: String::new(),
            date: None,
            body_text: Some("reply body".to_string()),
            body_html: None,
            headers,
        };
        assert_eq!(email.in_reply_to(), Some("<orig@example.com>"));
        assert_eq!(email.correlation(), Some("<orig@example.com>"));
        assert_eq!(email.best_body(), "reply body");
    }
}
