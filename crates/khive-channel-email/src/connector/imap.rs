//! IMAP inbound connector.
//!
//! Fetches new messages from the INBOX since a given timestamp. TLS is always
//! required; plaintext IMAP connections are rejected. Credentials are supplied
//! at construction time from environment variables.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use khive_channel::ChannelError;
use mail_parser::MessageParser;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::instrument;

use crate::backoff::ImapSingleFlight;
use crate::oauth::{TokenProvider, XOAuth2Authenticator};

use super::RawEmail;

/// IMAP session type using TLS over a compat-wrapped tokio stream.
type ImapSession = async_imap::Session<
    async_native_tls::TlsStream<tokio_util::compat::Compat<tokio::net::TcpStream>>,
>;

/// Durable IMAP poll progress: the mailbox's `UIDVALIDITY` epoch plus the
/// greatest UID durably handled within that epoch (issue #449).
///
/// `NonZeroU32` mirrors the wire invariant: IMAP's own `UIDVALIDITY` and
/// `UID` values are never zero (see [`validate_uid_validity`] and
/// [`select_uid_page`]'s zero-UID rejection).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImapProgress {
    pub(crate) uid_validity: Option<NonZeroU32>,
    pub(crate) last_seen_uid: Option<NonZeroU32>,
}

/// One page of IMAP fetch results plus the progress the caller should adopt
/// once the page is durably handled.
///
/// `emails` carries exactly one [`SelectedMessage`] per selected UID (khive
/// #449 High fix): every selected UID gets a durable terminal disposition
/// before `next_progress` is allowed to advance past it, so a single
/// permanently malformed message can never starve later UIDs.
pub(crate) struct ImapFetchPage {
    pub(crate) emails: Vec<SelectedMessage>,
    pub(crate) next_progress: ImapProgress,
}

/// Why a selected UID could not be parsed into a [`RawEmail`] (khive #449
/// High fix). Distinguishes the two permanent-failure shapes so the
/// downstream quarantine record carries an accurate reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MalformedReason {
    /// The `UID FETCH` response carried no RFC822 body for this UID.
    MissingBody,
    /// An RFC822 body was present but `mail_parser` could not parse it.
    ParseFailure,
}

impl std::fmt::Display for MalformedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            MalformedReason::MissingBody => "missing-body",
            MalformedReason::ParseFailure => "parse-failure",
        })
    }
}

/// The terminal disposition of one selected UID: either a successfully
/// parsed message, or a durable quarantine marker for a UID that could not
/// be parsed (khive #449 High fix). Every selected UID produces exactly one
/// of these -- never silently dropped, never left without a disposition.
pub(crate) enum SelectedMessage {
    Email(Box<RawEmail>),
    Malformed {
        uid: u32,
        /// Stable `imap:{host}:{uidvalidity}:{uid}` dedup key, precomputed
        /// here since a malformed message has no parsed headers to derive
        /// it from.
        imap_external_id: String,
        reason: MalformedReason,
    },
}

/// Internal trait for IMAP fetch operations.
///
/// Allows unit tests to substitute a mock without a live IMAP server.
#[async_trait]
pub(crate) trait ImapConnector: Send + Sync + 'static {
    /// Fetch a sorted, deduplicated, progress-bound page of messages.
    ///
    /// `progress` is the caller's last-known `(UIDVALIDITY, high-water)`
    /// pair; the connector never advances past an incompletely-handled page
    /// (see [`process_selected_page`]).
    async fn fetch_page(
        &self,
        since: DateTime<Utc>,
        limit: usize,
        progress: ImapProgress,
    ) -> Result<ImapFetchPage, ChannelError>;
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
    /// Per-credential single-flight guard (#605): at most one concurrent
    /// connection attempt for this `LiveImap` (i.e. this credential) proceeds;
    /// a bounded semaphore is the only way to widen the cap later.
    single_flight: ImapSingleFlight,
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
            single_flight: ImapSingleFlight::new(),
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
            single_flight: ImapSingleFlight::new(),
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
    async fn fetch_page(
        &self,
        since: DateTime<Utc>,
        limit: usize,
        progress: ImapProgress,
    ) -> Result<ImapFetchPage, ChannelError> {
        // Single-flight (#605): hold this credential's one permit for the
        // full connect-through-logout lifecycle below, so a second concurrent
        // call (e.g. a future caller widening the poll loop's concurrency)
        // waits for this connection to finish instead of opening a second one
        // against the same mailbox.
        let _permit = self.single_flight.acquire().await;
        let mut session = self.connect().await?;

        // SELECT returns the Mailbox struct; uid_validity is the UIDVALIDITY value needed
        // for stable per-message dedup keys of the form `imap:{host}:{uidvalidity}:{uid}`.
        let mailbox = session
            .select("INBOX")
            .await
            .map_err(|e| ChannelError::Transport(format!("IMAP SELECT INBOX failed: {e}")))?;
        let uid_validity = validate_uid_validity(mailbox.uid_validity)?;

        let Some(query) = uid_search_query(since, uid_validity, progress) else {
            // Current epoch's high-water is already at u32::MAX: nothing more
            // to search, and the epoch/high-water are unchanged.
            let _ = session.logout().await;
            return Ok(ImapFetchPage {
                emails: vec![],
                next_progress: progress,
            });
        };

        let uid_set = session
            .uid_search(query)
            .await
            .map_err(|e| ChannelError::Transport(format!("IMAP UID SEARCH failed: {e}")))?;

        let selected =
            select_uid_page(uid_set.into_iter().collect(), limit, uid_validity, progress)?;

        if selected.is_empty() {
            let _ = session.logout().await;
            let next = next_progress(uid_validity, progress, &selected);
            return Ok(ImapFetchPage {
                emails: vec![],
                next_progress: next,
            });
        }

        let uid_str = selected
            .iter()
            .map(|u| u.get().to_string())
            .collect::<Vec<_>>()
            .join(",");

        // Collect the fetch stream into owned bytes before releasing the session borrow.
        // Every fetch entry is kept, including a `None` body: filtering bodyless
        // entries here would hide an incomplete selected page from
        // `process_selected_page`, which must see (and reject) that gap.
        let fetched_raw: Vec<(Option<u32>, Option<Vec<u8>>)> = {
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
                collected.push((msg.uid, msg.body().map(|b| b.to_vec())));
            }
            collected
        };

        let _ = session.logout().await;

        let emails = process_selected_page(uid_validity, &selected, fetched_raw, &self.host)?;
        let next = next_progress(uid_validity, progress, &selected);
        Ok(ImapFetchPage {
            emails,
            next_progress: next,
        })
    }
}

/// Reject a missing or zero `UIDVALIDITY` before any search/fetch is issued.
///
/// UIDVALIDITY is required to form the stable `imap:{host}:{uidvalidity}:{uid}`
/// dedup key and to bind durable high-water progress to the correct epoch;
/// without it we cannot safely identify, deduplicate, or page messages.
fn validate_uid_validity(uid_validity: Option<u32>) -> Result<NonZeroU32, ChannelError> {
    uid_validity.and_then(NonZeroU32::new).ok_or_else(|| {
        ChannelError::Transport(
            "IMAP SELECT did not return a valid UIDVALIDITY; \
             cannot safely deduplicate messages — poll aborted"
                .to_string(),
        )
    })
}

/// Build the `UID SEARCH` criteria string for the current epoch and progress.
///
/// Returns `None` when the current epoch matches stored progress and the
/// high-water is already `u32::MAX` (the epoch is exhausted; no search is
/// issued). Otherwise: same epoch with a known high-water searches strictly
/// above it (`UID {h+1}:*`); a new/changed epoch, or a same epoch with no
/// high-water yet, searches by date (`SINCE <since>`), ignoring any high-water
/// from a different epoch.
fn uid_search_query(
    since: DateTime<Utc>,
    current_uid_validity: NonZeroU32,
    progress: ImapProgress,
) -> Option<String> {
    if progress.uid_validity == Some(current_uid_validity) {
        match progress.last_seen_uid {
            Some(h) if h.get() == u32::MAX => None,
            Some(h) => Some(format!("UID {}:*", h.get().saturating_add(1))),
            None => Some(format!("SINCE {}", since.format("%d-%b-%Y"))),
        }
    } else {
        Some(format!("SINCE {}", since.format("%d-%b-%Y")))
    }
}

/// Normalize a raw `UID SEARCH` result into a bounded, deterministic page.
///
/// Order: reject any zero UID (protocol-invalid); sort ascending; deduplicate;
/// when the epoch matches stored progress, retain only UIDs strictly above the
/// high-water as a defensive re-check; then truncate to `limit`.
fn select_uid_page(
    mut uids: Vec<u32>,
    limit: usize,
    current_uid_validity: NonZeroU32,
    progress: ImapProgress,
) -> Result<Vec<NonZeroU32>, ChannelError> {
    if uids.contains(&0) {
        return Err(ChannelError::Transport(
            "IMAP UID SEARCH returned a zero UID, which is protocol-invalid".to_string(),
        ));
    }
    uids.sort_unstable();
    uids.dedup();

    if progress.uid_validity == Some(current_uid_validity) {
        if let Some(high_water) = progress.last_seen_uid {
            uids.retain(|&u| u > high_water.get());
        }
    }

    uids.truncate(limit);

    Ok(uids
        .into_iter()
        .map(|u| NonZeroU32::new(u).expect("zero UIDs already rejected above"))
        .collect())
}

/// Compute the checkpoint candidate after selecting (not yet validating) a page.
///
/// A non-empty selection advances to `(current epoch, max selected UID)`. An
/// empty selection under an unchanged epoch leaves `prior` untouched (nothing
/// to commit — avoids a write every poll). An empty selection under a new or
/// changed epoch still replaces the obsolete epoch, with `high_water: None`,
/// so a stale epoch's high-water is never silently reused.
fn next_progress(
    current_uid_validity: NonZeroU32,
    prior: ImapProgress,
    selected: &[NonZeroU32],
) -> ImapProgress {
    match selected.iter().map(|u| u.get()).max() {
        Some(max) => ImapProgress {
            uid_validity: Some(current_uid_validity),
            last_seen_uid: NonZeroU32::new(max),
        },
        None if prior.uid_validity == Some(current_uid_validity) => prior,
        None => ImapProgress {
            uid_validity: Some(current_uid_validity),
            last_seen_uid: None,
        },
    }
}

/// Validate a fully-fetched selected page and build the [`SelectedMessage`]
/// list, in `selected_uids` order — exactly one entry per selected UID. A
/// gap or duplicate fails the whole page (no partial advancement); a missing
/// or unparseable body gets a durable `Malformed` disposition instead
/// (khive #449) so the caller can quarantine it and advance past it.
/// See `crates/khive-channel-email/docs/api/imap-connector.md`.
pub(crate) fn process_selected_page(
    uid_validity: NonZeroU32,
    selected_uids: &[NonZeroU32],
    fetched_raw: Vec<(Option<u32>, Option<Vec<u8>>)>,
    host: &str,
) -> Result<Vec<SelectedMessage>, ChannelError> {
    let mut by_uid: HashMap<u32, Option<Vec<u8>>> = HashMap::new();
    for (uid_opt, body_opt) in fetched_raw {
        let Some(uid) = uid_opt.filter(|&u| u != 0) else {
            continue;
        };
        if !selected_uids.iter().any(|s| s.get() == uid) {
            tracing::warn!(host = %host, uid, "ignoring unrequested IMAP fetch response");
            continue;
        }
        if by_uid.insert(uid, body_opt).is_some() {
            return Err(ChannelError::Transport(format!(
                "IMAP UID FETCH returned duplicate responses for selected UID {uid}"
            )));
        }
    }

    let mut result = Vec::with_capacity(selected_uids.len());
    for &uid in selected_uids {
        let uid = uid.get();
        let imap_external_id = format!("imap:{host}:{}:{uid}", uid_validity.get());
        let entry = by_uid.remove(&uid).ok_or_else(|| {
            ChannelError::Transport(format!(
                "IMAP UID FETCH did not return a response for selected UID {uid}; \
                 page rejected, not partially advanced"
            ))
        })?;
        let message = match entry {
            None => {
                tracing::warn!(
                    host = %host,
                    uid,
                    "IMAP UID FETCH returned no RFC822 body for selected UID; quarantining"
                );
                SelectedMessage::Malformed {
                    uid,
                    imap_external_id,
                    reason: MalformedReason::MissingBody,
                }
            }
            Some(raw) => match parse_raw_bytes(uid, &raw, host, uid_validity.get()) {
                Some(email) => SelectedMessage::Email(Box::new(email)),
                None => {
                    tracing::warn!(
                        host = %host,
                        uid,
                        "failed to parse RFC822 bytes for selected UID; quarantining"
                    );
                    SelectedMessage::Malformed {
                        uid,
                        imap_external_id,
                        reason: MalformedReason::ParseFailure,
                    }
                }
            },
        };
        result.push(message);
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

    // Collect headers into a flat lowercase map (first occurrence wins). Id-list
    // headers (Message-ID, In-Reply-To, References, ...) route through
    // mail-parser's `parse_id`, which returns `HeaderValue::Text` for exactly one
    // id but `HeaderValue::TextList` for two or more (mail-parser 0.9.4
    // `parsers/fields/id.rs`) -- a `References` chain with an ancestor beyond the
    // immediate parent is exactly that shape. Both variants are joined into a
    // single RFC 5322 whitespace-separated value here so the chain survives;
    // every other header shape (addresses, dates, ...) is dropped exactly as
    // before.
    let mut headers: HashMap<String, String> = HashMap::new();
    for header in msg.headers() {
        let key = header.name().to_lowercase();
        if let std::collections::hash_map::Entry::Vacant(e) = headers.entry(key) {
            match header.value() {
                mail_parser::HeaderValue::Text(v) => {
                    e.insert(v.to_string());
                }
                mail_parser::HeaderValue::TextList(list) => {
                    e.insert(
                        list.iter()
                            .map(|v| v.as_ref())
                            .collect::<Vec<_>>()
                            .join(" "),
                    );
                }
                _ => {}
            }
        }
    }

    // Every Authentication-Results occurrence, in document order (topmost
    // first), verbatim. Collected separately from `headers` above because that
    // map keeps only the first occurrence of a header name; the attribution
    // gate needs every occurrence to find the topmost one carrying the
    // configured trust anchor (ADR-056 Amendment 2026-07-02).
    let mut authentication_results: Vec<String> = Vec::new();
    for header in msg.headers() {
        if !header.name().eq_ignore_ascii_case("authentication-results") {
            continue;
        }
        match header.value() {
            mail_parser::HeaderValue::Text(v) => authentication_results.push(v.to_string()),
            mail_parser::HeaderValue::TextList(list) => authentication_results.push(
                list.iter()
                    .map(|v| v.as_ref())
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => {}
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
        authentication_results,
    })
}

/// IMAP fetcher wrapping an `ImapConnector`.
pub struct ImapFetcher {
    pub(crate) inner: Arc<dyn ImapConnector>,
    /// In-memory compatibility cursor for [`Self::fetch_since`] callers (e.g.
    /// direct library use outside the daemon poll loop, and this crate's own
    /// tests). Progress-based within one process, but never persisted — the
    /// daemon's durable checkpoint path goes through [`Self::fetch_page`]
    /// with an explicit [`ImapProgress`] loaded from storage instead.
    legacy_progress: tokio::sync::Mutex<ImapProgress>,
}

impl ImapFetcher {
    /// Create a production fetcher using basic IMAP LOGIN credentials.
    pub fn new(host: impl Into<String>, port: u16, username: &str, password: &str) -> Self {
        Self {
            inner: Arc::new(LiveImap::new(host, port, username, password)),
            legacy_progress: tokio::sync::Mutex::new(ImapProgress::default()),
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
            legacy_progress: tokio::sync::Mutex::new(ImapProgress::default()),
        }
    }

    /// Create a fetcher wrapping a custom connector (for testing).
    #[cfg(test)]
    pub(crate) fn with_connector(connector: impl ImapConnector) -> Self {
        Self {
            inner: Arc::new(connector),
            legacy_progress: tokio::sync::Mutex::new(ImapProgress::default()),
        }
    }

    /// Fetch messages received since `since`, up to `limit` items.
    ///
    /// Maintains an in-memory `(UIDVALIDITY, high-water)` cursor across calls
    /// on this `ImapFetcher` instance, advancing it only when the underlying
    /// fetch succeeds. This makes repeated standalone calls progress-based
    /// within one process without requiring a durable checkpoint; the daemon
    /// poll loop uses [`Self::fetch_page`] with an explicit stored checkpoint
    /// instead.
    pub(crate) async fn fetch_since(
        &self,
        since: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<SelectedMessage>, ChannelError> {
        let mut progress = self.legacy_progress.lock().await;
        let page = self.inner.fetch_page(since, limit, *progress).await?;
        *progress = page.next_progress;
        Ok(page.emails)
    }

    /// Fetch one explicit-progress page (crate-internal: used by
    /// `EmailChannel::poll_page` with a durable checkpoint loaded from
    /// storage, bypassing the in-memory `legacy_progress` cursor entirely).
    pub(crate) async fn fetch_page(
        &self,
        since: DateTime<Utc>,
        limit: usize,
        progress: ImapProgress,
    ) -> Result<ImapFetchPage, ChannelError> {
        self.inner.fetch_page(since, limit, progress).await
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
        async fn fetch_page(
            &self,
            _since: DateTime<Utc>,
            _limit: usize,
            _progress: ImapProgress,
        ) -> Result<ImapFetchPage, ChannelError> {
            Ok(ImapFetchPage {
                emails: self
                    .emails
                    .clone()
                    .into_iter()
                    .map(|e| SelectedMessage::Email(Box::new(e)))
                    .collect(),
                next_progress: ImapProgress::default(),
            })
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
            authentication_results: Vec::new(),
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
        let SelectedMessage::Email(email) = &result[0] else {
            panic!("expected a parsed email, got a malformed disposition");
        };
        assert_eq!(email.imap_external_id, "imap:mail.example.com:12345:1");
        assert_eq!(email.from_addrs, vec!["alice@example.com"]);
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
    fn parse_raw_bytes_preserves_multi_id_references() {
        // mail-parser 0.9.4's `parse_id` returns `HeaderValue::TextList` (not
        // `Text`) once a header holds 2+ ids -- a `References` chain with an
        // ancestor beyond the immediate parent is exactly that shape. The raw
        // parse path must not drop it down to a single (or zero) ids.
        let raw = b"From: alice@example.com\r\n\
                    To: me@example.com\r\n\
                    Subject: Multi-id References test\r\n\
                    References: <grandparent1@example.com> <grandparent2@example.com>\r\n\
                    \r\n\
                    body";
        let email = parse_raw_bytes(9, raw, "imap.example.com", 4242).unwrap();
        assert_eq!(
            email.references(),
            Some("grandparent1@example.com grandparent2@example.com"),
            "both ancestor ids must survive the raw IMAP parse, not just the first"
        );
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
    fn parse_raw_bytes_strips_angle_brackets_from_real_in_reply_to() {
        // Real RFC 822 bytes routed through mail_parser (not a synthetic
        // RawEmail/headers map like the tests below) — pins the bracket-
        // stripping behavior that comm.ingest's bracket-toggle matching
        // depends on. mail_parser's id parser (used for
        // In-Reply-To/Message-ID/References) strips the `<...>` envelope
        // for the single-ancestor case, so the headers map must carry the
        // bracket-free form.
        let raw = b"From: alice@example.com\r\n\
                    To: me@example.com\r\n\
                    Subject: Re: original thread\r\n\
                    Message-ID: <reply-id@khive.ai>\r\n\
                    In-Reply-To: <some-id@khive.ai>\r\n\
                    Date: Wed, 1 Jul 2026 10:00:00 +0000\r\n\
                    \r\n\
                    This is a reply.";
        let email = parse_raw_bytes(11, raw, "imap.example.com", 42).unwrap();
        assert_eq!(
            email.headers.get("in-reply-to").map(String::as_str),
            Some("some-id@khive.ai"),
            "mail_parser must strip angle brackets for the single-ancestor case"
        );
        assert_eq!(
            email.correlation(),
            Some("some-id@khive.ai"),
            "correlation() falls through to in_reply_to when X-Khive-Thread-ID is absent"
        );
    }

    #[test]
    fn parse_raw_bytes_correlation_prefers_khive_thread_id_when_present() {
        let raw = b"From: alice@example.com\r\n\
                    To: me@example.com\r\n\
                    Subject: Re: original thread\r\n\
                    Message-ID: <reply-id@khive.ai>\r\n\
                    In-Reply-To: <some-id@khive.ai>\r\n\
                    X-Khive-Thread-ID: thread-uuid-123\r\n\
                    Date: Wed, 1 Jul 2026 10:00:00 +0000\r\n\
                    \r\n\
                    This is a reply.";
        let email = parse_raw_bytes(12, raw, "imap.example.com", 42).unwrap();
        assert_eq!(
            email.correlation(),
            Some("thread-uuid-123"),
            "correlation() must prefer X-Khive-Thread-ID over In-Reply-To"
        );
    }

    #[test]
    fn parse_raw_bytes_collects_single_authentication_results_header() {
        let raw = b"Authentication-Results: mx.example.com; dmarc=pass header.from=example.com\r\n\
                    From: maintainer@example.com\r\n\
                    To: me@example.com\r\n\
                    Subject: single AR header\r\n\
                    \r\n\
                    body";
        let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
        assert_eq!(
            email.authentication_results,
            vec!["mx.example.com; dmarc=pass header.from=example.com".to_string()]
        );
    }

    #[test]
    fn parse_raw_bytes_preserves_all_authentication_results_in_document_order() {
        // The general `headers` map keeps only the first occurrence of a header
        // name; Authentication-Results needs every occurrence, in the order an
        // MTA stamps them (topmost = most recently added), to support the
        // "topmost trusted authserv-id wins" attribution gate.
        let raw = b"Authentication-Results: mx.example.com; dmarc=pass header.from=example.com\r\n\
                    Authentication-Results: mx.example.com; dmarc=fail header.from=example.com\r\n\
                    From: maintainer@example.com\r\n\
                    To: me@example.com\r\n\
                    Subject: two AR headers\r\n\
                    \r\n\
                    body";
        let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
        assert_eq!(
            email.authentication_results,
            vec![
                "mx.example.com; dmarc=pass header.from=example.com".to_string(),
                "mx.example.com; dmarc=fail header.from=example.com".to_string(),
            ],
            "both occurrences must survive, topmost first"
        );
    }

    #[test]
    fn parse_raw_bytes_no_authentication_results_header_yields_empty_vec() {
        let raw = b"From: maintainer@example.com\r\n\
                    To: me@example.com\r\n\
                    Subject: no auth header\r\n\
                    \r\n\
                    body";
        let email = parse_raw_bytes(1, raw, "imap.example.com", 1).unwrap();
        assert!(email.authentication_results.is_empty());
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
            authentication_results: Vec::new(),
        };
        assert_eq!(email.khive_thread_id(), Some("some-uuid"));
        assert_eq!(email.correlation(), Some("some-uuid"));
    }

    // --- UID progress/paging pure-helper tests (issue #449) ---

    fn minimal_rfc822(from: &str, subject: &str) -> Vec<u8> {
        format!("From: {from}\r\nTo: me@example.com\r\nSubject: {subject}\r\n\r\nbody").into_bytes()
    }

    #[test]
    fn validate_uid_validity_rejects_missing_and_zero() {
        assert!(validate_uid_validity(None).is_err());
        assert!(validate_uid_validity(Some(0)).is_err());
        assert!(validate_uid_validity(Some(1)).is_ok());
    }

    #[test]
    fn uid_page_progress_drains_75_uids_in_50_then_25() {
        // Reversed with duplicates, mirroring async-imap's unordered HashSet result.
        let mut uids: Vec<u32> = (1..=75).rev().collect();
        uids.extend(1..=10);
        let validity = NonZeroU32::new(99).unwrap();
        let progress = ImapProgress::default();

        let page1 = select_uid_page(uids.clone(), 50, validity, progress).unwrap();
        assert_eq!(
            page1.iter().map(|u| u.get()).collect::<Vec<_>>(),
            (1u32..=50).collect::<Vec<_>>(),
            "first page must be exactly 1..=50"
        );

        let progress2 = next_progress(validity, progress, &page1);
        assert_eq!(progress2.last_seen_uid, NonZeroU32::new(50));

        let page2 = select_uid_page(uids.clone(), 50, validity, progress2).unwrap();
        assert_eq!(
            page2.iter().map(|u| u.get()).collect::<Vec<_>>(),
            (51u32..=75).collect::<Vec<_>>(),
            "second page must be exactly 51..=75, no overlap with the first"
        );

        let progress3 = next_progress(validity, progress2, &page2);
        let page3 = select_uid_page(uids, 50, validity, progress3).unwrap();
        assert!(
            page3.is_empty(),
            "third page must be empty; the backlog is fully drained"
        );
    }

    #[test]
    fn uid_page_is_independent_of_input_order_and_deduplicates_before_limit() {
        let validity = NonZeroU32::new(1).unwrap();
        let progress = ImapProgress::default();
        let ascending: Vec<u32> = (1..=10).collect();
        let mut shuffled = ascending.clone();
        shuffled.reverse();
        let mut with_dupes = shuffled.clone();
        with_dupes.extend(shuffled.clone());

        let a = select_uid_page(ascending, 10, validity, progress).unwrap();
        let b = select_uid_page(shuffled, 10, validity, progress).unwrap();
        let c = select_uid_page(with_dupes, 10, validity, progress).unwrap();
        assert_eq!(a, b, "page selection must not depend on input order");
        assert_eq!(a, c, "duplicate UIDs must not consume extra page slots");
    }

    #[test]
    fn uid_page_rejects_zero_uid() {
        let validity = NonZeroU32::new(1).unwrap();
        let result = select_uid_page(vec![0, 1, 2], 10, validity, ImapProgress::default());
        assert!(
            result.is_err(),
            "a protocol-invalid zero UID must reject the whole selection"
        );
    }

    #[test]
    fn same_uidvalidity_query_starts_strictly_above_high_water() {
        let validity = NonZeroU32::new(99).unwrap();
        let progress = ImapProgress {
            uid_validity: Some(validity),
            last_seen_uid: NonZeroU32::new(50),
        };
        assert_eq!(
            uid_search_query(Utc::now(), validity, progress).as_deref(),
            Some("UID 51:*")
        );
    }

    #[test]
    fn uidvalidity_change_uses_since_and_ignores_old_high_water() {
        let old_validity = NonZeroU32::new(99).unwrap();
        let new_validity = NonZeroU32::new(100).unwrap();
        let progress = ImapProgress {
            uid_validity: Some(old_validity),
            last_seen_uid: NonZeroU32::new(50),
        };
        let since = DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            uid_search_query(since, new_validity, progress).as_deref(),
            Some("SINCE 01-Jul-2026")
        );

        // The new epoch's selection must not filter by the old epoch's high-water.
        let selected = select_uid_page(vec![3, 1, 2], 50, new_validity, progress).unwrap();
        assert_eq!(
            selected.iter().map(|u| u.get()).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "new epoch selection must retain low UIDs the old epoch's high-water would have excluded"
        );
    }

    #[test]
    fn max_uid_high_water_issues_no_search_for_same_epoch() {
        let validity = NonZeroU32::new(99).unwrap();
        let progress = ImapProgress {
            uid_validity: Some(validity),
            last_seen_uid: NonZeroU32::new(u32::MAX),
        };
        assert_eq!(
            uid_search_query(Utc::now(), validity, progress),
            None,
            "an exhausted epoch (high-water at u32::MAX) must not repeat the search"
        );
    }

    #[test]
    fn next_progress_changed_epoch_empty_selection_resets_high_water_to_none() {
        let old_validity = NonZeroU32::new(99).unwrap();
        let new_validity = NonZeroU32::new(100).unwrap();
        let prior = ImapProgress {
            uid_validity: Some(old_validity),
            last_seen_uid: NonZeroU32::new(50),
        };
        let next = next_progress(new_validity, prior, &[]);
        assert_eq!(
            next,
            ImapProgress {
                uid_validity: Some(new_validity),
                last_seen_uid: None,
            },
            "a UIDVALIDITY change with an empty selection must still adopt the new \
             epoch and reset high-water, not silently keep the old epoch's value"
        );
    }

    #[test]
    fn next_progress_same_epoch_empty_selection_keeps_prior_unchanged() {
        let validity = NonZeroU32::new(99).unwrap();
        let prior = ImapProgress {
            uid_validity: Some(validity),
            last_seen_uid: NonZeroU32::new(50),
        };
        let next = next_progress(validity, prior, &[]);
        assert_eq!(
            next, prior,
            "an empty page under an unchanged epoch must leave the checkpoint \
             byte-identical, avoiding a wasted commit every poll"
        );
    }

    #[test]
    fn select_uid_page_dedup_happens_before_truncate_so_limit_is_not_wasted_on_duplicates() {
        let validity = NonZeroU32::new(1).unwrap();
        let mut uids = vec![1u32; 200];
        uids.extend(2..=40); // 39 more distinct UIDs -> 40 distinct total.
        let selected = select_uid_page(uids, 50, validity, ImapProgress::default()).unwrap();
        assert_eq!(
            selected.iter().map(|u| u.get()).collect::<Vec<_>>(),
            (1u32..=40).collect::<Vec<_>>(),
            "200 duplicate copies of UID 1 must not consume page slots that belong \
             to the 39 genuinely distinct UIDs"
        );
    }

    #[test]
    fn select_uid_page_repeated_overlapping_search_result_yields_only_new_uids() {
        let validity = NonZeroU32::new(1).unwrap();
        let progress = ImapProgress {
            uid_validity: Some(validity),
            last_seen_uid: NonZeroU32::new(10),
        };
        // A broad re-search (e.g. after a retry) that re-includes already-consumed
        // UIDs 1..=10 alongside genuinely new UIDs 11..=15.
        let uids: Vec<u32> = (1..=15).collect();
        let selected = select_uid_page(uids, 50, validity, progress).unwrap();
        assert_eq!(
            selected.iter().map(|u| u.get()).collect::<Vec<_>>(),
            (11u32..=15).collect::<Vec<_>>(),
            "a re-searched page that overlaps already-consumed UIDs must yield only \
             the strictly-new tail"
        );
    }

    #[test]
    fn select_uid_page_out_of_order_uids_with_gaps_selected_in_sorted_order() {
        let validity = NonZeroU32::new(1).unwrap();
        let uids = vec![97, 3, 500, 12, 3, 8, 250];
        let selected = select_uid_page(uids, 50, validity, ImapProgress::default()).unwrap();
        assert_eq!(
            selected.iter().map(|u| u.get()).collect::<Vec<_>>(),
            vec![3, 8, 12, 97, 250, 500],
            "sparse, unordered, duplicated UIDs must sort and dedupe correctly, not \
             just over a contiguous range"
        );
    }

    #[test]
    fn strict_page_orders_fetch_responses_by_selected_uid() {
        let validity = NonZeroU32::new(1).unwrap();
        let selected = vec![
            NonZeroU32::new(1).unwrap(),
            NonZeroU32::new(2).unwrap(),
            NonZeroU32::new(3).unwrap(),
        ];
        // Fetch stream returns entries in reverse order.
        let fetched = vec![
            (Some(3), Some(minimal_rfc822("c@example.com", "three"))),
            (Some(2), Some(minimal_rfc822("b@example.com", "two"))),
            (Some(1), Some(minimal_rfc822("a@example.com", "one"))),
        ];
        let emails =
            process_selected_page(validity, &selected, fetched, "imap.example.com").unwrap();
        let uids: Vec<u32> = emails
            .iter()
            .map(|m| match m {
                SelectedMessage::Email(e) => e.uid,
                SelectedMessage::Malformed { uid, .. } => *uid,
            })
            .collect();
        assert_eq!(
            uids,
            vec![1, 2, 3],
            "output must follow selected_uids order, not fetch response order"
        );
    }

    #[test]
    fn strict_page_missing_body_quarantines_without_failing_page() {
        // UID 2 is present in the response but carries no RFC822 body -- must
        // get a durable Malformed disposition, not fail the whole page
        // (khive #449 High fix: a permanently bodyless message must not
        // starve every later UID).
        let validity = NonZeroU32::new(1).unwrap();
        let selected = vec![NonZeroU32::new(1).unwrap(), NonZeroU32::new(2).unwrap()];
        let missing_body = vec![
            (Some(1), Some(minimal_rfc822("a@example.com", "one"))),
            (Some(2), None),
        ];
        let result = process_selected_page(validity, &selected, missing_body, "h").unwrap();
        assert_eq!(result.len(), 2, "both selected UIDs must get a disposition");
        assert!(
            matches!(&result[0], SelectedMessage::Email(e) if e.uid == 1),
            "UID 1 must parse normally"
        );
        assert!(
            matches!(
                &result[1],
                SelectedMessage::Malformed {
                    uid: 2,
                    reason: MalformedReason::MissingBody,
                    ..
                }
            ),
            "UID 2 must be quarantined as missing-body, not dropped or erroring"
        );
    }

    #[test]
    fn strict_page_absent_uid_still_errors() {
        // UID 2 never appears in the fetch response at all -- a genuine
        // protocol gap (distinct from a malformed body), so the whole page
        // still fails; a self-healing retry is safe because an expunged
        // message will not be re-selected next poll.
        let validity = NonZeroU32::new(1).unwrap();
        let selected = vec![NonZeroU32::new(1).unwrap(), NonZeroU32::new(2).unwrap()];
        let missing_uid = vec![(Some(1), Some(minimal_rfc822("a@example.com", "one")))];
        assert!(
            process_selected_page(validity, &selected, missing_uid, "h").is_err(),
            "a selected UID absent from the fetch response must fail the whole page"
        );
    }

    #[test]
    fn strict_page_parse_failure_quarantines_without_failing_page() {
        let validity = NonZeroU32::new(1).unwrap();
        let selected = vec![NonZeroU32::new(1).unwrap()];
        // Empty body — mail_parser returns None ("if no headers are found
        // None is returned").
        let fetched = vec![(Some(1), Some(Vec::new()))];
        let result = process_selected_page(validity, &selected, fetched, "h").unwrap();
        assert_eq!(result.len(), 1);
        assert!(
            matches!(
                &result[0],
                SelectedMessage::Malformed {
                    uid: 1,
                    reason: MalformedReason::ParseFailure,
                    ..
                }
            ),
            "malformed selected RFC822 bytes must quarantine, not fail the whole page \
             or silently advance without a record"
        );
    }

    #[test]
    fn strict_page_ignores_unrequested_fetch_response() {
        let validity = NonZeroU32::new(1).unwrap();
        let selected = vec![NonZeroU32::new(1).unwrap()];
        let fetched = vec![
            (Some(1), Some(minimal_rfc822("a@example.com", "one"))),
            // UID 99 was never selected; a stray response for it must be ignored,
            // not treated as part of the page.
            (Some(99), Some(minimal_rfc822("stray@example.com", "stray"))),
        ];
        let emails = process_selected_page(validity, &selected, fetched, "h").unwrap();
        assert_eq!(emails.len(), 1);
        assert!(matches!(&emails[0], SelectedMessage::Email(e) if e.uid == 1));
    }

    #[tokio::test]
    async fn mock_imap_backlog_drains_across_fetch_since_calls() {
        // Simulates async-imap search always returning the full 75-message
        // backlog (as it would every 5s until the SINCE window advances) --
        // ImapFetcher's legacy_progress cursor must still drain it as
        // 50, then 25, then 0 across repeated standalone calls.
        struct BacklogMockImap {
            validity: NonZeroU32,
            all_uids: Vec<u32>,
        }

        #[async_trait]
        impl ImapConnector for BacklogMockImap {
            async fn fetch_page(
                &self,
                _since: DateTime<Utc>,
                limit: usize,
                progress: ImapProgress,
            ) -> Result<ImapFetchPage, ChannelError> {
                let selected =
                    select_uid_page(self.all_uids.clone(), limit, self.validity, progress)?;
                let fetched: Vec<(Option<u32>, Option<Vec<u8>>)> = selected
                    .iter()
                    .map(|u| {
                        (
                            Some(u.get()),
                            Some(minimal_rfc822("sender@example.com", "backlog")),
                        )
                    })
                    .collect();
                let emails =
                    process_selected_page(self.validity, &selected, fetched, "mail.example.com")?;
                let next = next_progress(self.validity, progress, &selected);
                Ok(ImapFetchPage {
                    emails,
                    next_progress: next,
                })
            }
        }

        let mut all_uids: Vec<u32> = (1..=75).rev().collect();
        all_uids.extend(1..=5); // duplicates, mirroring HashSet noise

        let fetcher = ImapFetcher::with_connector(BacklogMockImap {
            validity: NonZeroU32::new(42).unwrap(),
            all_uids,
        });

        let page1 = fetcher.fetch_since(Utc::now(), 50).await.unwrap();
        assert_eq!(page1.len(), 50);
        let page2 = fetcher.fetch_since(Utc::now(), 50).await.unwrap();
        assert_eq!(page2.len(), 25);
        let page3 = fetcher.fetch_since(Utc::now(), 50).await.unwrap();
        assert!(
            page3.is_empty(),
            "backlog must be fully drained after two pages"
        );
    }

    #[test]
    fn one_poison_uid_does_not_starve_51_later_valid_uids_and_cursor_passes_it() {
        // khive #449 High regression: a single permanently malformed UID
        // (here, UID 1: an empty body mail_parser cannot parse) followed by
        // 50 valid messages. All 50 valid ones must parse; the poison UID
        // must get a durable quarantine disposition (not silently dropped,
        // not failing the page); and the candidate high-water must advance
        // past the poison UID along with everything else, so the next poll
        // never re-selects it.
        let validity = NonZeroU32::new(1).unwrap();
        let selected: Vec<NonZeroU32> = (1u32..=51).map(|u| NonZeroU32::new(u).unwrap()).collect();

        let mut fetched: Vec<(Option<u32>, Option<Vec<u8>>)> = vec![(Some(1), Some(Vec::new()))];
        fetched.extend(
            (2u32..=51).map(|u| (Some(u), Some(minimal_rfc822("sender@example.com", "valid")))),
        );

        let result = process_selected_page(validity, &selected, fetched, "h").unwrap();
        assert_eq!(result.len(), 51, "every selected UID gets a disposition");

        assert!(
            matches!(
                &result[0],
                SelectedMessage::Malformed {
                    uid: 1,
                    reason: MalformedReason::ParseFailure,
                    ..
                }
            ),
            "the poison UID must carry a quarantine record, not be dropped"
        );
        let valid_uids: Vec<u32> = result[1..]
            .iter()
            .map(|m| match m {
                SelectedMessage::Email(e) => e.uid,
                SelectedMessage::Malformed { uid, .. } => *uid,
            })
            .collect();
        assert_eq!(
            valid_uids,
            (2u32..=51).collect::<Vec<_>>(),
            "all 50 valid messages after the poison UID must still ingest"
        );

        let next = next_progress(validity, ImapProgress::default(), &selected);
        assert_eq!(
            next.last_seen_uid,
            NonZeroU32::new(51),
            "the checkpoint candidate must advance past the poison UID, not stall on it"
        );
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
            authentication_results: Vec::new(),
        };
        assert_eq!(email.in_reply_to(), Some("<orig@example.com>"));
        assert_eq!(email.correlation(), Some("<orig@example.com>"));
        assert_eq!(email.best_body(), "reply body");
    }
}
