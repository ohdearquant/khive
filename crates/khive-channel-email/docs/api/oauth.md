# OAuth2 token provider and XOAUTH2 SASL encoding

Source: `crates/khive-channel-email/src/oauth.rs`. Covers the client-credentials token
provider used by both connectors and the XOAUTH2 SASL wire encoding they authenticate with.

## Token endpoint

The SMTP and IMAP connectors both use `KHIVE_EMAIL_OAUTH_CLIENT_ID` (when set) to fetch an
app-only token from Microsoft's v2 client-credentials endpoint:
`POST https://login.microsoftonline.com/{tenant_id}/oauth2/v2.0/token`, scope fixed to
`https://outlook.office365.com/.default` for Exchange Online.

`fetch_token` performs a live HTTP request and must not be called from unit tests without a
network mock — test the pure helpers (`xoauth2_sasl`, `XOAuth2Authenticator::process`)
instead.

## `TokenProvider`

Thread-safe OAuth2 token cache with a 60-second early-refresh margin. Both the SMTP and IMAP
connectors hold an `Arc<TokenProvider>` so a single fetch serves both connections. A
`tokio::sync::Mutex` serializes token refreshes: the cache-hit path acquires the lock, reads,
and releases; the cache-miss/expired-token refresh path is bounded by `TOKEN_REFRESH_TIMEOUT`
while the lock is held, and a timeout releases the lock so a subsequent call can retry rather
than deadlocking concurrent callers.

`get_token` fetches a new token whenever the cached one has less than 60 seconds of life
remaining (`refresh_at`, computed as `expires_in - 60s` from the server-reported value).

## Token response validation

`validate_token_response` fails closed before a freshly-deserialized token response is
cached: rejects an empty `access_token`, control characters in the token string (which would
corrupt the XOAUTH2 SASL payload), a zero `expires_in`, and a `token_type` that is not
`"Bearer"` when the field is present (providers may omit the field entirely).

## Error code allowlisting

The token endpoint's raw response body is never interpolated into a returned error — it may
contain credential echoes, HTML, proxy injection text, or CRLF sequences usable for log
forging. `ALLOWED_OAUTH_ERROR_CODES` allowlists the OAuth 2.0 error codes from RFC 6749 §5.2
and Microsoft AADSTS; any code from the token endpoint not in this list is replaced with the
opaque indicator `"oauth_error"`.

## `xoauth2_sasl` (test-only reference implementation)

Production code uses `lettre` and `async-imap` directly, not this helper — it exists in tests
to verify the wire encoding matches what those libraries actually send:
`user=<mailbox>\x01auth=Bearer <token>\x01\x01`, base64-encoded.

- **SMTP**: `lettre`'s `Mechanism::Xoauth2` constructs the same raw bytes internally via
  `Credentials::new(mailbox, access_token)`.
- **IMAP**: `XOAuth2Authenticator::process` returns the **raw** (non-base64) bytes;
  async-imap 0.9 base64-encodes them before sending (see `do_auth_handshake` in async-imap
  `client.rs:282`).

Both paths are therefore wire-equivalent even though neither calls `xoauth2_sasl` directly.

### Worked test vector

`raw = b"user=u@h\x01auth=Bearer t\x01\x01"` (24 bytes, no padding). Grouping into 3-byte
chunks and mapping through the base64 alphabet (`A`=0..25, `a`=26..51, `0`=52..61):

```
[75 73 65] -> dXNl   [72 3d 75] -> cj11   [40 68 01] -> QGgB
[61 75 74] -> YXV0   [68 3d 42] -> aD1C   [65 61 72] -> ZWFy
```

## `XOAuth2Authenticator`

IMAP `Authenticator` implementation for XOAUTH2. `process` is called by async-imap with the
decoded server challenge and returns the raw SASL bytes; the framework base64-encodes them
before sending (async-imap 0.9 `do_auth_handshake`, `client.rs:282`).
