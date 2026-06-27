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

/// Production IMAP connector backed by `async-imap` with native TLS.
pub(crate) struct LiveImap {
    host: String,
    port: u16,
    username: String,
    password: String,
}

impl LiveImap {
    pub(crate) fn new(
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            username: username.into(),
            password: password.into(),
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

        // Wrap with compat layer so async-native-tls (which uses futures-io) can use the stream.
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

        // IMAP login with 15s timeout.
        let client = async_imap::Client::new(tls_stream);
        let session: ImapSession = tokio::time::timeout(
            Duration::from_secs(15),
            client.login(&self.username, &self.password),
        )
        .await
        .map_err(|_| ChannelError::Auth("IMAP login timed out (15s)".into()))?
        .map_err(|(e, _)| ChannelError::Auth(format!("IMAP login failed: {e}")))?;

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

        session
            .select("INBOX")
            .await
            .map_err(|e| ChannelError::Transport(format!("IMAP SELECT INBOX failed: {e}")))?;

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
        let fetched_raw: Vec<(u32, Vec<u8>)> = {
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
                let uid = msg.uid.unwrap_or(0);
                if let Some(body) = msg.body() {
                    collected.push((uid, body.to_vec()));
                }
            }
            collected
        };

        let _ = session.logout().await;

        let mut result = Vec::new();
        for (uid, raw_bytes) in fetched_raw {
            if let Some(email) = parse_raw_bytes(uid, &raw_bytes) {
                result.push(email);
            }
        }
        Ok(result)
    }
}

/// Parse raw RFC 822 bytes into a `RawEmail`.
pub(crate) fn parse_raw_bytes(uid: u32, raw: &[u8]) -> Option<RawEmail> {
    let parser = MessageParser::default();
    let msg = parser.parse(raw)?;

    let from = msg
        .from()
        .and_then(|a| a.first())
        .and_then(|a| a.address())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();

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

    let message_id = msg.message_id().map(|s| s.to_string());

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
        message_id,
        from,
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
    /// Create a production fetcher backed by `async-imap`.
    pub fn new(host: impl Into<String>, port: u16, username: &str, password: &str) -> Self {
        Self {
            inner: Arc::new(LiveImap::new(host, port, username, password)),
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

    fn make_email(uid: u32, msg_id: &str, from: &str) -> RawEmail {
        RawEmail {
            uid,
            message_id: Some(msg_id.to_string()),
            from: from.to_string(),
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
        let emails = vec![make_email(1, "<id1@example.com>", "alice@example.com")];
        let fetcher = ImapFetcher::with_connector(MockImap::with_emails(emails));
        let result = fetcher.fetch_since(Utc::now(), 50).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].message_id.as_deref(), Some("<id1@example.com>"));
    }

    #[tokio::test]
    async fn mock_imap_returns_empty_when_no_messages() {
        let fetcher = ImapFetcher::with_connector(MockImap::with_emails(vec![]));
        let result = fetcher.fetch_since(Utc::now(), 50).await.unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn raw_email_khive_thread_id_header() {
        let mut headers = HashMap::new();
        headers.insert("x-khive-thread-id".to_string(), "some-uuid".to_string());
        let email = RawEmail {
            uid: 1,
            message_id: None,
            from: "a@example.com".to_string(),
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

    #[test]
    fn raw_email_in_reply_to_fallback() {
        let mut headers = HashMap::new();
        headers.insert("in-reply-to".to_string(), "<orig@example.com>".to_string());
        let email = RawEmail {
            uid: 2,
            message_id: None,
            from: "b@example.com".to_string(),
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
