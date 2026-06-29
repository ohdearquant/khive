//! OAuth2 client-credentials token provider and XOAUTH2 SASL helper.
//!
//! Used by the SMTP and IMAP connectors when `KHIVE_EMAIL_OAUTH_CLIENT_ID` is
//! set. The token endpoint is Microsoft's app-only flow:
//! `POST https://login.microsoftonline.com/{tenant_id}/oauth2/v2.0/token`.

use std::time::{Duration, Instant};

#[cfg(test)]
use base64::Engine as _;
use khive_channel::ChannelError;
use tokio::sync::Mutex;

// ──────────────────────────────────────────────────── XOAUTH2 SASL helpers

/// Return the XOAUTH2 SASL payload as a standard base64 string.
///
/// The raw SASL bytes are:
/// ```text
/// user=<mailbox>\x01auth=Bearer <token>\x01\x01
/// ```
/// which are base64-encoded before being sent on the wire.
///
/// For SMTP, `lettre`'s `Mechanism::Xoauth2` constructs the same raw bytes
/// internally via `Credentials::new(mailbox, access_token)`.
///
/// For IMAP, `XOAuth2Authenticator::process` returns the **raw** bytes;
/// async-imap 0.9 base64-encodes them before sending (see `do_auth_handshake`
/// in async-imap client.rs:282).  Both paths are therefore wire-equivalent.
///
/// This helper is a reference implementation used in tests to verify the
/// wire encoding.  Production code uses lettre and async-imap directly.
#[cfg(test)]
pub(crate) fn xoauth2_sasl(mailbox: &str, token: &str) -> String {
    let raw = format!("user={mailbox}\x01auth=Bearer {token}\x01\x01");
    base64::engine::general_purpose::STANDARD.encode(raw.as_bytes())
}

/// IMAP `Authenticator` implementation for XOAUTH2.
///
/// `process` is called by async-imap with the decoded server challenge.
/// It returns the **raw** SASL bytes; the framework base64-encodes them before
/// sending (async-imap 0.9 `do_auth_handshake`, client.rs:282).
pub(crate) struct XOAuth2Authenticator {
    pub(crate) mailbox: String,
    pub(crate) token: String,
}

impl async_imap::Authenticator for XOAuth2Authenticator {
    type Response = Vec<u8>;

    fn process(&mut self, _challenge: &[u8]) -> Vec<u8> {
        format!(
            "user={}\x01auth=Bearer {}\x01\x01",
            self.mailbox, self.token
        )
        .into_bytes()
    }
}

// ─────────────────────────────────────────────────── token provider cache

/// A cached OAuth2 access token together with its expiry deadline.
struct CachedToken {
    access_token: String,
    /// Earliest `Instant` at which a refresh should be attempted (60 s before
    /// the server-reported `expires_in`).
    expires_at: Instant,
}

/// Thread-safe OAuth2 token provider with a 60-second early-refresh margin.
///
/// Both the SMTP and IMAP connectors hold an `Arc<TokenProvider>` so a single
/// fetch serves both connections.  A `tokio::sync::Mutex` serialises token
/// refreshes; the cache-hit path acquires the lock, reads, and releases.
pub struct TokenProvider {
    tenant_id: String,
    client_id: String,
    client_secret: String,
    cached: Mutex<Option<CachedToken>>,
}

impl TokenProvider {
    /// Create a new provider. No network call is made until `get_token`.
    pub fn new(tenant_id: String, client_id: String, client_secret: String) -> Self {
        Self {
            tenant_id,
            client_id,
            client_secret,
            cached: Mutex::new(None),
        }
    }

    /// Return a valid access token, fetching a new one when the cached token
    /// has less than 60 seconds of life remaining.
    pub async fn get_token(&self) -> Result<String, ChannelError> {
        let mut guard = self.cached.lock().await;
        if let Some(ref cached) = *guard {
            if cached.expires_at > Instant::now() {
                return Ok(cached.access_token.clone());
            }
        }
        let resp = fetch_token(&self.tenant_id, &self.client_id, &self.client_secret).await?;
        let expires_at = Instant::now() + Duration::from_secs(resp.expires_in.saturating_sub(60));
        *guard = Some(CachedToken {
            access_token: resp.access_token.clone(),
            expires_at,
        });
        Ok(resp.access_token)
    }
}

// ──────────────────────────────────────── Microsoft token endpoint request

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

/// Fetch an app-only OAuth2 token from Microsoft's v2 client-credentials endpoint.
///
/// Scope is fixed to `https://outlook.office365.com/.default` for Exchange Online.
/// This function performs a live HTTP request and must not be called from unit
/// tests without a network mock.  Test the pure helpers (`xoauth2_sasl`,
/// `XOAuth2Authenticator::process`) instead.
async fn fetch_token(
    tenant_id: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<TokenResponse, ChannelError> {
    let url = format!("https://login.microsoftonline.com/{tenant_id}/oauth2/v2.0/token");
    let params = [
        ("grant_type", "client_credentials"),
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("scope", "https://outlook.office365.com/.default"),
    ];
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .form(&params)
        .send()
        .await
        .map_err(|e| ChannelError::Auth(format!("OAuth2 token request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(ChannelError::Auth(format!(
            "OAuth2 token endpoint returned {status}: {body}"
        )));
    }

    resp.json::<TokenResponse>()
        .await
        .map_err(|e| ChannelError::Auth(format!("OAuth2 token response parse failed: {e}")))
}

// ───────────────────────────────────────────────────────────── tests

#[cfg(test)]
mod tests {
    use async_imap::Authenticator as _;

    use super::*;

    /// Verify `xoauth2_sasl` against a manually computed base64 test vector.
    ///
    /// raw = b"user=u@h\x01auth=Bearer t\x01\x01" (24 bytes, no padding)
    ///
    /// Groups of 3 → base64 chars (A=0 .. Z=25, a=26 .. z=51, 0=52 .. 9=61):
    ///   [75 73 65] → dXNl   [72 3d 75] → cj11   [40 68 01] → QGgB
    ///   [61 75 74] → YXV0   [68 3d 42] → aD1C   [65 61 72] → ZWFy
    ///   [65 72 20] → ZXIg   [74 01 01] → dAEB
    /// Result: "dXNlcj11QGgBYXV0aD1CZWFyZXIgdAEB"
    #[test]
    fn xoauth2_sasl_known_vector() {
        let got = xoauth2_sasl("u@h", "t");
        assert_eq!(got, "dXNlcj11QGgBYXV0aD1CZWFyZXIgdAEB");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&got)
            .expect("must be valid base64");
        assert_eq!(decoded, b"user=u@h\x01auth=Bearer t\x01\x01" as &[u8]);
    }

    /// Verify that `XOAuth2Authenticator::process` returns the unencoded SASL
    /// bytes that async-imap will then base64-encode.
    #[test]
    fn authenticator_process_returns_raw_bytes() {
        let mut auth = XOAuth2Authenticator {
            mailbox: "m@x.io".to_string(),
            token: "fake-token".to_string(),
        };
        let raw = auth.process(&[]);
        // Must be the raw (NOT base64) SASL string.
        assert_eq!(
            raw,
            b"user=m@x.io\x01auth=Bearer fake-token\x01\x01" as &[u8]
        );
        // The raw bytes are exactly the base64-decode of what xoauth2_sasl returns.
        let b64 = xoauth2_sasl("m@x.io", "fake-token");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .unwrap();
        assert_eq!(
            raw, decoded,
            "process() bytes must equal decode(xoauth2_sasl())"
        );
    }
}
