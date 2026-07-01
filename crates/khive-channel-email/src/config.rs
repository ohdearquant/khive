//! Environment-only configuration for the email channel.
//!
//! All settings are read from environment variables. No filesystem config is
//! loaded. Missing required variables produce an error at construction time.

use crate::connector::MailAddress;
use khive_channel::ChannelError;

/// Authentication mode for SMTP and IMAP connections.
///
/// Selected at construction time based on the environment variables that are
/// present:
/// - If `KHIVE_EMAIL_OAUTH_CLIENT_ID` is set → `OAuth` mode (tenant_id and
///   client_secret are also required; missing any one of the three is an error).
/// - Otherwise → `Basic` mode (password is required).
///
/// `Debug` output redacts `password` and `client_secret`.
#[derive(Clone)]
pub enum EmailAuth {
    /// Username + password (standard SMTP AUTH PLAIN / IMAP LOGIN).
    Basic {
        /// Login password. Never logged or stored beyond this struct.
        password: String,
    },
    /// App-only OAuth2 client-credentials flow for Microsoft Exchange Online.
    OAuth {
        /// Azure AD tenant ID.
        tenant_id: String,
        /// Application (client) ID registered in Azure AD.
        client_id: String,
        /// Client secret. Never logged or stored beyond this struct.
        client_secret: String,
    },
}

impl std::fmt::Debug for EmailAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmailAuth::Basic { .. } => f
                .debug_struct("EmailAuth::Basic")
                .field("password", &"<redacted>")
                .finish(),
            EmailAuth::OAuth {
                tenant_id,
                client_id,
                ..
            } => f
                .debug_struct("EmailAuth::OAuth")
                .field("tenant_id", tenant_id)
                .field("client_id", client_id)
                .field("client_secret", &"<redacted>")
                .finish(),
        }
    }
}

/// Configuration for the SMTP sender and IMAP fetcher.
///
/// Build via [`EmailChannelConfig::from_env`]. All fields are sourced
/// exclusively from environment variables; no defaults are provided for
/// credentials or host names.
///
/// `Debug` output redacts credentials; see the manual `Debug` impl on
/// [`EmailAuth`].
#[derive(Clone, Debug)]
pub struct EmailChannelConfig {
    /// SMTP relay host (e.g. `smtp.example.com`).
    pub smtp_host: String,
    /// SMTP port. Defaults to 587 (STARTTLS) when `KHIVE_EMAIL_SMTP_PORT` is unset.
    pub smtp_port: u16,
    /// IMAP server host (e.g. `imap.example.com`).
    pub imap_host: String,
    /// IMAP port. Defaults to 993 (TLS) when `KHIVE_EMAIL_IMAP_PORT` is unset.
    pub imap_port: u16,
    /// Login username for SMTP AUTH / IMAP LOGIN (used in `Basic` mode).
    pub username: String,
    /// Mailbox address used as the `user=` field in the XOAUTH2 SASL string.
    ///
    /// Defaults to `username` when `KHIVE_EMAIL_MAILBOX` is not set.
    pub mailbox: String,
    /// Authentication credentials and mode.
    pub auth: EmailAuth,
    /// The authorized maintainer address(es). Inbound messages from any other
    /// sender are rejected before ingestion. Populated from a comma-separated
    /// `KHIVE_EMAIL_MAINTAINER_ADDRESS`; the first entry is the primary, used for
    /// outbound-allowlist defaulting and envelope `to` fallback. Never empty.
    pub maintainer_addresses: Vec<MailAddress>,
}

impl EmailChannelConfig {
    /// Load configuration from environment variables.
    ///
    /// Required variables (always):
    /// - `KHIVE_EMAIL_SMTP_HOST`
    /// - `KHIVE_EMAIL_IMAP_HOST`
    /// - `KHIVE_EMAIL_USERNAME`
    /// - `KHIVE_EMAIL_MAINTAINER_ADDRESS`
    ///
    /// Auth-mode selection:
    /// - If `KHIVE_EMAIL_OAUTH_CLIENT_ID` is present → OAuth mode.
    ///   Also requires `KHIVE_EMAIL_OAUTH_TENANT_ID` and
    ///   `KHIVE_EMAIL_OAUTH_CLIENT_SECRET`; a clear error is returned when
    ///   only a subset is set.
    /// - Otherwise → Basic mode. `KHIVE_EMAIL_PASSWORD` is required.
    ///
    /// Optional variables with defaults:
    /// - `KHIVE_EMAIL_SMTP_PORT` (default `587`)
    /// - `KHIVE_EMAIL_IMAP_PORT` (default `993`)
    /// - `KHIVE_EMAIL_MAILBOX` (default: same as `KHIVE_EMAIL_USERNAME`)
    pub fn from_env() -> Result<Self, ChannelError> {
        let smtp_host = require_env("KHIVE_EMAIL_SMTP_HOST")?;
        let smtp_port = optional_port("KHIVE_EMAIL_SMTP_PORT", 587)?;
        let imap_host = require_env("KHIVE_EMAIL_IMAP_HOST")?;
        let imap_port = optional_port("KHIVE_EMAIL_IMAP_PORT", 993)?;
        let username = require_env("KHIVE_EMAIL_USERNAME")?;
        validate_sasl_string(&username, "KHIVE_EMAIL_USERNAME")?;

        let mailbox = match std::env::var("KHIVE_EMAIL_MAILBOX") {
            Ok(v) if !v.is_empty() => {
                validate_sasl_string(&v, "KHIVE_EMAIL_MAILBOX")?;
                v
            }
            _ => username.clone(), // already validated above
        };

        let auth = build_auth()?;

        // Comma-separated allowlist so the maintainer can register more than one
        // address (e.g. both a Gmail and an Outlook account). First entry is primary.
        let maintainer_raw = require_env("KHIVE_EMAIL_MAINTAINER_ADDRESS")?;
        let maintainer_addresses = maintainer_raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                MailAddress::parse(s).ok_or_else(|| {
                    ChannelError::Config(format!(
                        "KHIVE_EMAIL_MAINTAINER_ADDRESS contains an invalid RFC 5322 address: {s:?}"
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if maintainer_addresses.is_empty() {
            return Err(ChannelError::Config(
                "KHIVE_EMAIL_MAINTAINER_ADDRESS must contain at least one address".into(),
            ));
        }

        Ok(Self {
            smtp_host,
            smtp_port,
            imap_host,
            imap_port,
            username,
            mailbox,
            auth,
            maintainer_addresses,
        })
    }
}

/// Determine the auth mode from environment variables.
///
/// If `KHIVE_EMAIL_OAUTH_CLIENT_ID` is set, OAuth mode is selected and all
/// three OAuth vars are required.  If only one or two are present an error is
/// returned; partial config is never silently accepted.
fn build_auth() -> Result<EmailAuth, ChannelError> {
    let client_id = std::env::var("KHIVE_EMAIL_OAUTH_CLIENT_ID").ok();
    let tenant_id = std::env::var("KHIVE_EMAIL_OAUTH_TENANT_ID").ok();
    let client_secret = std::env::var("KHIVE_EMAIL_OAUTH_CLIENT_SECRET").ok();

    match (tenant_id, client_id, client_secret) {
        // All three OAuth vars present → OAuth mode.
        (Some(tid), Some(cid), Some(cs)) => Ok(EmailAuth::OAuth {
            tenant_id: tid,
            client_id: cid,
            client_secret: cs,
        }),
        // None present → Basic mode.
        (None, None, None) => {
            let password = require_env("KHIVE_EMAIL_PASSWORD")?;
            Ok(EmailAuth::Basic { password })
        }
        // Partial OAuth config → clear error.
        (tid, cid, cs) => {
            let mut missing = Vec::new();
            if tid.is_none() {
                missing.push("KHIVE_EMAIL_OAUTH_TENANT_ID");
            }
            if cid.is_none() {
                missing.push("KHIVE_EMAIL_OAUTH_CLIENT_ID");
            }
            if cs.is_none() {
                missing.push("KHIVE_EMAIL_OAUTH_CLIENT_SECRET");
            }
            Err(ChannelError::Config(format!(
                "partial OAuth configuration: missing {missing:?}; \
                 set all three of KHIVE_EMAIL_OAUTH_TENANT_ID, \
                 KHIVE_EMAIL_OAUTH_CLIENT_ID, and KHIVE_EMAIL_OAUTH_CLIENT_SECRET, \
                 or set none of them to use basic auth"
            )))
        }
    }
}

fn require_env(key: &str) -> Result<String, ChannelError> {
    std::env::var(key).map_err(|_| {
        ChannelError::Config(format!("required environment variable {key:?} is not set"))
    })
}

fn optional_port(key: &str, default: u16) -> Result<u16, ChannelError> {
    match std::env::var(key) {
        Err(_) => Ok(default),
        Ok(v) => v.parse::<u16>().map_err(|_| {
            ChannelError::Config(format!(
                "environment variable {key:?} must be a valid port number (1-65535), got: {v:?}"
            ))
        }),
    }
}

/// Validate a string for use in the XOAUTH2 SASL `user=` field.
///
/// Rejects:
/// - empty strings
/// - strings without `@` (i.e. not an RFC 5322 addr-spec)
/// - any ASCII control character (U+0000–U+001F, U+007F), including the
///   `\x01` delimiter used in the SASL payload and the `\r\n` terminators
///   used in IMAP/SMTP line framing
///
/// Called at config construction time so control characters can never reach
/// the XOAUTH2 SASL payload builder or the IMAP/SMTP connectors.
fn validate_sasl_string(value: &str, field_name: &str) -> Result<(), ChannelError> {
    if value.is_empty() {
        return Err(ChannelError::Config(format!(
            "{field_name} must not be empty"
        )));
    }
    if !value.contains('@') {
        return Err(ChannelError::Config(format!(
            "{field_name} must be a valid email address (no '@' found)"
        )));
    }
    if value.chars().any(|c| matches!(c, '\x00'..='\x1f' | '\x7f')) {
        return Err(ChannelError::Config(format!(
            "{field_name} contains disallowed control characters"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    // ── Env-mutation serialization (Finding 3) ────────────────────────────────
    //
    // Environment variables are process-global state.  Tests that set or remove
    // them must not run concurrently or they observe each other's mutations.
    //
    // Strategy: a crate-local `ENV_MUTEX` serialises every env-mutating test.
    // An `EnvSnapshot` captures prior values at test entry and restores them
    // on drop — even if the test panics — so the mutex lock always releases
    // with a clean env.

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    /// RAII snapshot of environment variables.  On drop, each variable is
    /// restored to its value at capture time (or removed if it was absent).
    struct EnvSnapshot {
        vars: Vec<(String, Option<String>)>,
    }

    impl EnvSnapshot {
        fn capture(keys: &[&str]) -> Self {
            Self {
                vars: keys
                    .iter()
                    .map(|k| (k.to_string(), std::env::var(k).ok()))
                    .collect(),
            }
        }
    }

    impl Drop for EnvSnapshot {
        fn drop(&mut self) {
            for (k, v) in &self.vars {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    // ── Pure-function tests (no env mutation) ─────────────────────────────────

    #[test]
    fn missing_required_env_returns_error() {
        std::env::remove_var("KHIVE_EMAIL_SMTP_HOST_TEST_MISSING");
        let result = require_env("KHIVE_EMAIL_SMTP_HOST_TEST_MISSING");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("KHIVE_EMAIL_SMTP_HOST_TEST_MISSING"));
    }

    #[test]
    fn debug_output_does_not_expose_password() {
        let auth = EmailAuth::Basic {
            password: "super-secret-credential-99".to_string(), // gitleaks:allow
        };
        let debug_output = format!("{auth:?}");
        assert!(
            !debug_output.contains("super-secret-credential-99"),
            "Debug output must not expose the password; got: {debug_output:?}"
        );
        assert!(
            debug_output.contains("<redacted>"),
            "Debug output must include the redaction marker; got: {debug_output:?}"
        );
    }

    #[test]
    fn debug_output_does_not_expose_client_secret() {
        let auth = EmailAuth::OAuth {
            tenant_id: "fake-tenant".to_string(),
            client_id: "fake-client-id".to_string(),
            client_secret: "ultra-secret-client-secret-99".to_string(), // gitleaks:allow
        };
        let debug_output = format!("{auth:?}");
        assert!(
            !debug_output.contains("ultra-secret-client-secret-99"),
            "Debug output must not expose the client_secret; got: {debug_output:?}"
        );
        assert!(
            debug_output.contains("<redacted>"),
            "Debug output must include the redaction marker; got: {debug_output:?}"
        );
        // tenant_id and client_id should be visible (not sensitive).
        assert!(
            debug_output.contains("fake-tenant"),
            "tenant_id should be visible in debug; got: {debug_output:?}"
        );
        assert!(
            debug_output.contains("fake-client-id"),
            "client_id should be visible in debug; got: {debug_output:?}"
        );
    }

    // ── Finding 2: validate_sasl_string ──────────────────────────────────────

    #[test]
    fn validate_sasl_string_rejects_control_char_in_mailbox() {
        // \x01 is the XOAUTH2 SASL delimiter and must be rejected.
        let err = validate_sasl_string("user\x01@example.com", "mailbox").unwrap_err();
        assert!(
            err.to_string().contains("control characters"),
            "expected control-char rejection, got: {err}"
        );
    }

    #[test]
    fn validate_sasl_string_rejects_crlf_in_mailbox() {
        let err = validate_sasl_string("user@example.com\r\n", "mailbox").unwrap_err();
        assert!(err.to_string().contains("control characters"));
    }

    #[test]
    fn validate_sasl_string_rejects_missing_at_sign() {
        let err = validate_sasl_string("notanemail", "username").unwrap_err();
        assert!(err.to_string().contains("'@'"));
    }

    #[test]
    fn validate_sasl_string_rejects_empty() {
        let err = validate_sasl_string("", "username").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn validate_sasl_string_accepts_valid_email() {
        assert!(validate_sasl_string("mailbox@example.com", "username").is_ok());
    }

    // ── optional_port tests (env-mutating — serialized with ENV_MUTEX) ────────

    #[test]
    fn optional_port_uses_default_when_absent() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _snap = EnvSnapshot::capture(&["KHIVE_EMAIL_SMTP_PORT_TEST_DEFAULT"]);
        std::env::remove_var("KHIVE_EMAIL_SMTP_PORT_TEST_DEFAULT");
        let port = optional_port("KHIVE_EMAIL_SMTP_PORT_TEST_DEFAULT", 587).unwrap();
        assert_eq!(port, 587);
    }

    #[test]
    fn optional_port_parses_set_value() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _snap = EnvSnapshot::capture(&["KHIVE_EMAIL_SMTP_PORT_TEST2"]);
        std::env::set_var("KHIVE_EMAIL_SMTP_PORT_TEST2", "465");
        let port = optional_port("KHIVE_EMAIL_SMTP_PORT_TEST2", 587).unwrap();
        assert_eq!(port, 465);
    }

    #[test]
    fn optional_port_rejects_invalid_value() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _snap = EnvSnapshot::capture(&["KHIVE_EMAIL_SMTP_PORT_TEST3"]);
        std::env::set_var("KHIVE_EMAIL_SMTP_PORT_TEST3", "notaport");
        let result = optional_port("KHIVE_EMAIL_SMTP_PORT_TEST3", 587);
        assert!(result.is_err());
    }

    // ── build_auth tests (env-mutating — serialized with ENV_MUTEX) ───────────

    const TID: &str = "KHIVE_EMAIL_OAUTH_TENANT_ID";
    const CID: &str = "KHIVE_EMAIL_OAUTH_CLIENT_ID";
    const CS: &str = "KHIVE_EMAIL_OAUTH_CLIENT_SECRET";
    const PW: &str = "KHIVE_EMAIL_PASSWORD";

    /// Temporarily set all three OAuth vars and confirm OAuth variant is returned.
    #[test]
    fn build_auth_oauth_all_vars_present() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _snap = EnvSnapshot::capture(&[TID, CID, CS]);

        std::env::set_var(TID, "fake-tenant");
        std::env::set_var(CID, "fake-client-id");
        std::env::set_var(CS, "fake-secret"); // gitleaks:allow

        let result = build_auth();

        let auth = result.expect("all three OAuth vars set must succeed");
        assert!(
            matches!(auth, EmailAuth::OAuth { .. }),
            "expected OAuth variant"
        );
    }

    /// Only tenant_id and client_id present (no secret) → clear error.
    #[test]
    fn build_auth_partial_oauth_returns_error() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _snap = EnvSnapshot::capture(&[TID, CID, CS, PW]);

        std::env::set_var(TID, "fake-tenant");
        std::env::set_var(CID, "fake-client-id");
        std::env::remove_var(CS);
        // Ensure KHIVE_EMAIL_PASSWORD is also absent so we land in the partial
        // branch regardless.
        std::env::remove_var(PW);

        let result = build_auth();

        assert!(result.is_err(), "partial OAuth config must return an error");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("KHIVE_EMAIL_OAUTH_CLIENT_SECRET"),
            "error should name the missing variable; got: {msg}"
        );
    }

    /// No OAuth vars and KHIVE_EMAIL_PASSWORD set → Basic variant.
    #[test]
    fn build_auth_basic_no_oauth_vars() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _snap = EnvSnapshot::capture(&[TID, CID, CS, PW]);

        std::env::remove_var(TID);
        std::env::remove_var(CID);
        std::env::remove_var(CS);
        std::env::set_var(PW, "test-password");

        let result = build_auth();

        let auth = result.expect("no OAuth vars + password must succeed");
        assert!(
            matches!(auth, EmailAuth::Basic { .. }),
            "expected Basic variant"
        );
    }
}
