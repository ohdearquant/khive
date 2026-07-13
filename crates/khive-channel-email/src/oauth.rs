//! OAuth2 client-credentials token provider and XOAUTH2 SASL helper.
//!
//! Used by the SMTP and IMAP connectors when `KHIVE_EMAIL_OAUTH_CLIENT_ID` is
//! set. The token endpoint is Microsoft's app-only flow:
//! `POST https://login.microsoftonline.com/{tenant_id}/oauth2/v2.0/token`.

use std::{
    future::Future,
    time::{Duration, Instant},
};

#[cfg(test)]
use base64::Engine as _;
use khive_channel::ChannelError;
use tokio::sync::Mutex;

const TOKEN_REFRESH_TIMEOUT: Duration = Duration::from_secs(15);

// ──────────────────────────── OAuth error-body sanitization

/// Allowlisted OAuth 2.0 error codes from RFC 6749 §5.2 and Microsoft AADSTS.
///
/// Any code from the token endpoint that is not in this list is replaced with
/// the opaque indicator `"oauth_error"`.  The raw response body is **never**
/// interpolated into a returned error — it may contain credential echoes,
/// HTML, proxy injection text, or CRLF sequences usable for log forging.
const ALLOWED_OAUTH_ERROR_CODES: &[&str] = &[
    "access_denied",
    "consent_required",
    "interaction_required",
    "invalid_client",
    "invalid_grant",
    "invalid_request",
    "invalid_scope",
    "login_required",
    "server_error",
    "slow_down",
    "temporarily_unavailable",
    "unauthorized_client",
    "unsupported_grant_type",
];

/// Extract and allowlist an OAuth error code from a JSON error response body.
///
/// Parses the standardized `error` field from the response JSON.  If the field
/// is present and its value appears in [`ALLOWED_OAUTH_ERROR_CODES`], that
/// static string slice is returned.  Otherwise `"oauth_error"` is returned as
/// an opaque, safe indicator.
///
/// The raw `body` string is **never** returned or interpolated.
fn sanitize_oauth_error(body: &str) -> &'static str {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(code) = val.get("error").and_then(|v| v.as_str()) {
            if let Some(allowed) = ALLOWED_OAUTH_ERROR_CODES
                .iter()
                .copied()
                .find(|&c| c == code)
            {
                return allowed;
            }
        }
    }
    "oauth_error"
}

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
    ///
    /// The cache-miss/expired-token refresh is bounded by
    /// [`TOKEN_REFRESH_TIMEOUT`] while the cache lock is held; a timeout
    /// releases the lock so a subsequent call can retry.
    pub async fn get_token(&self) -> Result<String, ChannelError> {
        self.get_token_with_fetcher(|| {
            fetch_token(&self.tenant_id, &self.client_id, &self.client_secret)
        })
        .await
    }

    async fn get_token_with_fetcher<F, Fut>(&self, fetch: F) -> Result<String, ChannelError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<TokenResponse, ChannelError>>,
    {
        let mut guard = self.cached.lock().await;

        if let Some(cached) = guard.as_ref() {
            if cached.expires_at > Instant::now() {
                let access_token = cached.access_token.clone();
                drop(guard);
                return Ok(access_token);
            }
        }

        let resp = match tokio::time::timeout(TOKEN_REFRESH_TIMEOUT, fetch()).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(err)) => {
                drop(guard);
                return Err(err);
            }
            Err(_) => {
                drop(guard);
                return Err(ChannelError::Auth(format!(
                    "OAuth2 token refresh timed out ({TOKEN_REFRESH_TIMEOUT:?})"
                )));
            }
        };

        let expires_at = Instant::now() + Duration::from_secs(resp.expires_in.saturating_sub(60));
        *guard = Some(CachedToken {
            access_token: resp.access_token.clone(),
            expires_at,
        });
        drop(guard);

        Ok(resp.access_token)
    }
}

// ──────────────────────────────────────── Microsoft token endpoint request

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    /// Providers may omit this field; when present it must equal `"Bearer"`
    /// (case-insensitive).  Required by `validate_token_response`.
    #[serde(default)]
    token_type: Option<String>,
}

/// Validate a freshly-deserialized token response before caching it.
///
/// Fails closed: rejects an empty `access_token`, control characters in the
/// token string (which would corrupt the XOAUTH2 SASL payload), a zero
/// `expires_in`, and a `token_type` that is not `"Bearer"` when the field is
/// present.
fn validate_token_response(resp: &TokenResponse) -> Result<(), ChannelError> {
    if resp.access_token.is_empty() {
        return Err(ChannelError::Auth(
            "OAuth2 token response: access_token is empty".to_string(),
        ));
    }
    if resp
        .access_token
        .chars()
        .any(|c| matches!(c, '\x00'..='\x1f' | '\x7f'))
    {
        return Err(ChannelError::Auth(
            "OAuth2 token response: access_token contains disallowed control characters"
                .to_string(),
        ));
    }
    if resp.expires_in == 0 {
        return Err(ChannelError::Auth(
            "OAuth2 token response: expires_in must be positive".to_string(),
        ));
    }
    if let Some(ref tt) = resp.token_type {
        if !tt.eq_ignore_ascii_case("bearer") {
            return Err(ChannelError::Auth(
                "OAuth2 token response: token_type is not Bearer".to_string(),
            ));
        }
    }
    Ok(())
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
        // Read the body to extract a standardised error code; the raw body is
        // intentionally discarded — it may contain credential echoes, HTML, or
        // CRLF sequences usable for log injection.
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        let error_code = sanitize_oauth_error(&body);
        return Err(ChannelError::Auth(format!(
            "OAuth2 token endpoint returned HTTP {status}: {error_code}"
        )));
    }

    let token_resp = resp
        .json::<TokenResponse>()
        .await
        .map_err(|e| ChannelError::Auth(format!("OAuth2 token response parse failed: {e}")))?;
    validate_token_response(&token_resp)?;
    Ok(token_resp)
}

// ───────────────────────────────────────────────────────────── tests

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

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
            token: "fake-token".to_string(), // gitleaks:allow
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

    // ── Regression: sanitize_oauth_error must not leak the body ─────────────

    /// A token-endpoint error body containing credential-shaped strings, a CRLF
    /// sequence, and bearer-looking text must produce a sanitized indicator that
    /// contains none of those strings.
    #[test]
    fn sanitize_oauth_error_does_not_leak_body() {
        // Craft a body that looks like a real AADSTS error but also embeds strings
        // that must never appear in an error message returned to callers.
        let body = concat!(
            r#"{"error":"invalid_client","error_description":"AADSTS70011\r\n"#,
            r#"Authorization: Bearer leaked_bearer","access_token":"leaked_token_abc","#,
            r#""client_secret":"leaked_secret_xyz","token_type":"Bearer"}"#
        );
        let code = sanitize_oauth_error(body);

        // The returned code must come from the static allowlist only.
        assert!(
            !code.contains("access_token"),
            "must not contain 'access_token': {code:?}"
        );
        assert!(
            !code.contains("leaked_token_abc"),
            "must not contain token value: {code:?}"
        );
        assert!(
            !code.contains("client_secret"),
            "must not contain 'client_secret': {code:?}"
        );
        assert!(
            !code.contains("leaked_secret_xyz"),
            "must not contain secret value: {code:?}"
        );
        assert!(!code.contains("\r\n"), "must not contain CRLF: {code:?}");
        assert!(
            !code.contains("leaked_bearer"),
            "must not contain bearer-shaped text: {code:?}"
        );
        // The standardised error code is recognized and returned verbatim.
        assert_eq!(code, "invalid_client");
    }

    /// An error code that is not in the allowlist must be replaced with the
    /// opaque indicator, preventing injection of arbitrary strings.
    #[test]
    fn sanitize_oauth_error_rejects_unknown_codes() {
        let xss_body = r#"{"error":"<script>alert(1)</script>"}"#;
        assert_eq!(
            sanitize_oauth_error(xss_body),
            "oauth_error",
            "unknown/dangerous error code must fall back to 'oauth_error'"
        );

        let long_body = format!(r#"{{"error":"{}"}}"#, "x".repeat(512));
        assert_eq!(
            sanitize_oauth_error(&long_body),
            "oauth_error",
            "overlong error code must fall back to 'oauth_error'"
        );
    }

    /// A body that is not valid JSON must produce the opaque indicator.
    #[test]
    fn sanitize_oauth_error_handles_non_json_body() {
        assert_eq!(
            sanitize_oauth_error("<html>502 Bad Gateway</html>"),
            "oauth_error"
        );
        assert_eq!(sanitize_oauth_error(""), "oauth_error");
    }

    // ── validate_token_response rejects dangerous token values ─────────────

    /// A token that contains a control character (\x01 is the XOAUTH2 delimiter)
    /// must be rejected before being cached or used in a SASL payload.
    #[test]
    fn validate_token_response_rejects_control_char_in_access_token() {
        let resp = TokenResponse {
            access_token: "valid_prefix\x01injected".to_string(),
            expires_in: 3600,
            token_type: Some("Bearer".to_string()),
        };
        let err = validate_token_response(&resp).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("control characters"),
            "error must mention control characters: {msg}"
        );
    }

    /// An empty access_token must be rejected.
    #[test]
    fn validate_token_response_rejects_empty_access_token() {
        let resp = TokenResponse {
            access_token: String::new(),
            expires_in: 3600,
            token_type: None,
        };
        let err = validate_token_response(&resp).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    /// expires_in == 0 must be rejected.
    #[test]
    fn validate_token_response_rejects_zero_expires_in() {
        let resp = TokenResponse {
            access_token: "valid_token_abc".to_string(), // gitleaks:allow
            expires_in: 0,
            token_type: None,
        };
        let err = validate_token_response(&resp).unwrap_err();
        assert!(err.to_string().contains("positive"));
    }

    /// A non-Bearer token_type must be rejected when the field is present.
    #[test]
    fn validate_token_response_rejects_non_bearer_token_type() {
        let resp = TokenResponse {
            access_token: "valid_token_abc".to_string(), // gitleaks:allow
            expires_in: 3600,
            token_type: Some("mac".to_string()),
        };
        let err = validate_token_response(&resp).unwrap_err();
        assert!(err.to_string().contains("Bearer"));
    }

    /// A well-formed response (Bearer, positive expires_in, clean token) must pass.
    #[test]
    fn validate_token_response_accepts_valid_response() {
        let resp = TokenResponse {
            access_token: "eyJhbGciOiJSUzI1NiJ9.payload.sig".to_string(),
            expires_in: 3600,
            token_type: Some("Bearer".to_string()),
        };
        assert!(validate_token_response(&resp).is_ok());
    }

    // ── Issue #477: bounded refresh under the shared cache lock ─────────────

    /// A stalled cache-miss refresh must time out after [`TOKEN_REFRESH_TIMEOUT`],
    /// release the cache mutex, and let an already-queued caller acquire it and
    /// complete its own independently bounded refresh.
    #[tokio::test(start_paused = true)]
    async fn timed_out_refresh_releases_lock_and_waiting_retry_succeeds() {
        let provider = Arc::new(TokenProvider::new(
            "tenant".to_string(),
            "client".to_string(),
            "secret".to_string(),
        ));
        let (stalled_started_tx, stalled_started_rx) = tokio::sync::oneshot::channel();

        let stalled = {
            let provider = Arc::clone(&provider);
            tokio::spawn(async move {
                provider
                    .get_token_with_fetcher(move || {
                        stalled_started_tx
                            .send(())
                            .expect("stalled fetch start receiver must remain live");
                        std::future::pending::<Result<TokenResponse, ChannelError>>()
                    })
                    .await
            })
        };

        stalled_started_rx
            .await
            .expect("stalled refresh must acquire the lock and begin");
        assert!(
            provider.cached.try_lock().is_err(),
            "stalled refresh must still own the cache mutex"
        );

        let retry_fetch_calls = Arc::new(AtomicUsize::new(0));
        let (retry_started_tx, retry_started_rx) = tokio::sync::oneshot::channel();
        let retry = {
            let provider = Arc::clone(&provider);
            let retry_fetch_calls = Arc::clone(&retry_fetch_calls);
            tokio::spawn(async move {
                retry_started_tx
                    .send(())
                    .expect("retry start receiver must remain live");
                provider
                    .get_token_with_fetcher(move || {
                        retry_fetch_calls.fetch_add(1, Ordering::SeqCst);
                        std::future::ready(Ok(TokenResponse {
                            access_token: "retry_token".to_string(), // gitleaks:allow
                            expires_in: 3600,
                            token_type: Some("Bearer".to_string()),
                        }))
                    })
                    .await
            })
        };

        retry_started_rx
            .await
            .expect("retry caller must begin before the refresh deadline");
        assert_eq!(
            retry_fetch_calls.load(Ordering::SeqCst),
            0,
            "retry fetch must not start while the stalled owner holds the mutex"
        );

        tokio::time::advance(TOKEN_REFRESH_TIMEOUT - Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(
            !stalled.is_finished(),
            "refresh must remain pending before deadline"
        );
        assert_eq!(retry_fetch_calls.load(Ordering::SeqCst), 0);

        tokio::time::advance(Duration::from_secs(1)).await;
        let err = stalled
            .await
            .expect("stalled refresh task must not panic")
            .expect_err("stalled refresh must time out");
        match err {
            ChannelError::Auth(message) => {
                assert_eq!(message, "OAuth2 token refresh timed out (15s)");
            }
            other => panic!("expected ChannelError::Auth, got {other:?}"),
        }

        let token = tokio::time::timeout(Duration::from_secs(1), retry)
            .await
            .expect("queued retry must acquire the released cache mutex")
            .expect("retry task must not panic")
            .expect("retry refresh must succeed");
        assert_eq!(token, "retry_token");
        assert_eq!(retry_fetch_calls.load(Ordering::SeqCst), 1);
    }

    /// Two concurrent cache-miss callers must perform exactly one refresh; the
    /// second caller rechecks the populated cache after acquiring the lock and
    /// never invokes its own fetch closure.
    #[tokio::test(start_paused = true)]
    async fn concurrent_cache_miss_performs_one_refresh_and_shares_result() {
        let provider = Arc::new(TokenProvider::new(
            "tenant".to_string(),
            "client".to_string(),
            "secret".to_string(),
        ));
        let (fetch_started_tx, fetch_started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();

        let first = {
            let provider = Arc::clone(&provider);
            tokio::spawn(async move {
                provider
                    .get_token_with_fetcher(move || async move {
                        fetch_started_tx
                            .send(())
                            .expect("first fetch start receiver must remain live");
                        release_rx
                            .await
                            .expect("first fetch release sender must remain live");
                        Ok(TokenResponse {
                            access_token: "shared_token".to_string(), // gitleaks:allow
                            expires_in: 3600,
                            token_type: Some("Bearer".to_string()),
                        })
                    })
                    .await
            })
        };

        fetch_started_rx
            .await
            .expect("first refresh must acquire the lock and begin");
        assert!(
            provider.cached.try_lock().is_err(),
            "first refresh must still own the cache mutex"
        );

        let second_fetch_calls = Arc::new(AtomicUsize::new(0));
        let (second_started_tx, second_started_rx) = tokio::sync::oneshot::channel();
        let second = {
            let provider = Arc::clone(&provider);
            let second_fetch_calls = Arc::clone(&second_fetch_calls);
            tokio::spawn(async move {
                second_started_tx
                    .send(())
                    .expect("second caller start receiver must remain live");
                provider
                    .get_token_with_fetcher(move || {
                        second_fetch_calls.fetch_add(1, Ordering::SeqCst);
                        std::future::ready(Ok(TokenResponse {
                            access_token: "duplicate_token".to_string(), // gitleaks:allow
                            expires_in: 3600,
                            token_type: Some("Bearer".to_string()),
                        }))
                    })
                    .await
            })
        };

        second_started_rx
            .await
            .expect("second caller must begin while first refresh owns the lock");
        assert_eq!(
            second_fetch_calls.load(Ordering::SeqCst),
            0,
            "second fetch must wait behind the first refresh"
        );

        release_tx
            .send(())
            .expect("first refresh task must remain live");

        let first_token = tokio::time::timeout(Duration::from_secs(1), first)
            .await
            .expect("first refresh must finish after release")
            .expect("first refresh task must not panic")
            .expect("first refresh must succeed");
        let second_token = tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .expect("second caller must acquire the released cache mutex")
            .expect("second caller task must not panic")
            .expect("second caller must return the cached token");

        assert_eq!(first_token, "shared_token");
        assert_eq!(second_token, "shared_token");
        assert_eq!(second_fetch_calls.load(Ordering::SeqCst), 0);
    }

    fn success_resp(token: &str) -> TokenResponse {
        TokenResponse {
            access_token: token.to_string(),
            expires_in: 3600,
            token_type: Some("Bearer".to_string()),
        }
    }

    /// After one successful refresh populates the cache, a second call must
    /// return the cached token WITHOUT invoking its fetch closure at all.
    #[tokio::test(start_paused = true)]
    async fn cache_hit_no_contention_never_invokes_fetcher() {
        let provider = TokenProvider::new("t".into(), "c".into(), "s".into());

        let token = provider
            .get_token_with_fetcher(|| async { Ok(success_resp("hit_token")) })
            .await
            .expect("first refresh must succeed");
        assert_eq!(token, "hit_token");

        let second = provider
            .get_token_with_fetcher(|| async {
                panic!("cache-hit path must not invoke the fetcher");
                #[allow(unreachable_code)]
                Ok(success_resp("unused"))
            })
            .await
            .expect("cache hit must succeed without fetching");
        assert_eq!(second, "hit_token");
    }

    /// A fast (non-timeout) fetch failure must propagate the original error
    /// unchanged and release the lock immediately — not be reinterpreted as a
    /// timeout, and not leave the mutex held for the next caller.
    #[tokio::test(start_paused = true)]
    async fn refresh_failure_propagates_and_releases_lock_immediately() {
        let provider = Arc::new(TokenProvider::new("t".into(), "c".into(), "s".into()));

        let err = provider
            .get_token_with_fetcher(|| async {
                Err(ChannelError::Auth("invalid_client".to_string()))
            })
            .await
            .expect_err("fast failure must propagate");
        match err {
            ChannelError::Auth(msg) => assert_eq!(msg, "invalid_client"),
            other => panic!("expected ChannelError::Auth, got {other:?}"),
        }

        assert!(
            provider.cached.try_lock().is_ok(),
            "lock must be released synchronously after a fast fetch error"
        );
        let token = provider
            .get_token_with_fetcher(|| async { Ok(success_resp("after_failure")) })
            .await
            .expect("subsequent call must succeed");
        assert_eq!(token, "after_failure");
    }

    /// Proves the guard is released when the CALLER cancels/aborts the
    /// in-flight future — distinct from the internal `tokio::time::timeout`
    /// elapsing. No virtual time is advanced here: release must happen
    /// strictly from cancellation, not from [`TOKEN_REFRESH_TIMEOUT`] elapsing.
    #[tokio::test(start_paused = true)]
    async fn caller_cancellation_releases_lock_before_internal_timeout() {
        let provider = Arc::new(TokenProvider::new("t".into(), "c".into(), "s".into()));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();

        let handle = {
            let provider = Arc::clone(&provider);
            tokio::spawn(async move {
                provider
                    .get_token_with_fetcher(move || {
                        started_tx.send(()).expect("receiver must be live");
                        std::future::pending::<Result<TokenResponse, ChannelError>>()
                    })
                    .await
            })
        };

        started_rx
            .await
            .expect("task must start and acquire the lock");
        assert!(
            provider.cached.try_lock().is_err(),
            "in-flight refresh must hold the cache mutex"
        );

        // Cancel the CALLER well before TOKEN_REFRESH_TIMEOUT elapses. If the
        // lock were only released by the internal `tokio::time::timeout`
        // future, this abort alone would never free it.
        handle.abort();
        let join_result = handle.await;
        assert!(
            join_result.as_ref().is_err() && join_result.unwrap_err().is_cancelled(),
            "task must report as cancelled, not as having completed the pending fetch"
        );

        // Let the runtime run the aborted future's Drop glue.
        tokio::task::yield_now().await;

        assert!(
            provider.cached.try_lock().is_ok(),
            "cache mutex guard must be dropped when the caller's future is cancelled, \
             not only when the internal timeout elapses"
        );

        let token = provider
            .get_token_with_fetcher(|| async { Ok(success_resp("post_cancel")) })
            .await
            .expect("a fresh caller must be able to acquire and refresh after cancellation");
        assert_eq!(token, "post_cancel");
    }
}
