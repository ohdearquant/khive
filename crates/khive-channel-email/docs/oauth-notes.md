# OAuth2 / XOAUTH2 internals

Source: `crates/khive-channel-email/src/oauth.rs`

## `xoauth2_sasl` (test-only reference implementation)

Production code uses `lettre` and `async-imap` directly, not this helper — it exists in
tests to verify the wire encoding matches what those libraries actually send.

For SMTP, `lettre`'s `Mechanism::Xoauth2` constructs the same raw bytes internally via
`Credentials::new(mailbox, access_token)`.

For IMAP, `XOAuth2Authenticator::process` returns the **raw** bytes; async-imap 0.9
base64-encodes them before sending (see `do_auth_handshake` in async-imap
`client.rs:282`). Both paths are therefore wire-equivalent.
