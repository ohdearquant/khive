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
    /// The single authorized maintainer address. Inbound messages from any other
    /// sender are rejected before ingestion.
    pub maintainer_address: MailAddress,
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

        let mailbox = match std::env::var("KHIVE_EMAIL_MAILBOX") {
            Ok(v) if !v.is_empty() => v,
            _ => username.clone(),
        };

        let auth = build_auth()?;

        let maintainer_raw = require_env("KHIVE_EMAIL_MAINTAINER_ADDRESS")?;
        let maintainer_address = MailAddress::parse(&maintainer_raw).ok_or_else(|| {
            ChannelError::Config(format!(
                "KHIVE_EMAIL_MAINTAINER_ADDRESS is not a valid RFC 5322 address: {maintainer_raw:?}"
            ))
        })?;

        Ok(Self {
            smtp_host,
            smtp_port,
            imap_host,
            imap_port,
            username,
            mailbox,
            auth,
            maintainer_address,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_required_env_returns_error() {
        std::env::remove_var("KHIVE_EMAIL_SMTP_HOST_TEST_MISSING");
        let result = require_env("KHIVE_EMAIL_SMTP_HOST_TEST_MISSING");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("KHIVE_EMAIL_SMTP_HOST_TEST_MISSING"));
    }

    #[test]
    fn optional_port_uses_default_when_absent() {
        std::env::remove_var("KHIVE_EMAIL_SMTP_PORT_TEST");
        let port = optional_port("KHIVE_EMAIL_SMTP_PORT_TEST", 587).unwrap();
        assert_eq!(port, 587);
    }

    #[test]
    fn optional_port_parses_set_value() {
        std::env::set_var("KHIVE_EMAIL_SMTP_PORT_TEST2", "465");
        let port = optional_port("KHIVE_EMAIL_SMTP_PORT_TEST2", 587).unwrap();
        assert_eq!(port, 465);
        std::env::remove_var("KHIVE_EMAIL_SMTP_PORT_TEST2");
    }

    #[test]
    fn optional_port_rejects_invalid_value() {
        std::env::set_var("KHIVE_EMAIL_SMTP_PORT_TEST3", "notaport");
        let result = optional_port("KHIVE_EMAIL_SMTP_PORT_TEST3", 587);
        assert!(result.is_err());
        std::env::remove_var("KHIVE_EMAIL_SMTP_PORT_TEST3");
    }

    #[test]
    fn debug_output_does_not_expose_password() {
        let auth = EmailAuth::Basic {
            password: "super-secret-credential-99".to_string(),
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
            client_secret: "ultra-secret-client-secret-99".to_string(),
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

    // ── build_auth tests ────────────────────────────────────────────────────

    /// Temporarily set all three OAuth vars and confirm OAuth variant is returned.
    #[test]
    fn build_auth_oauth_all_vars_present() {
        // Isolate with unique suffix.
        const TID: &str = "KHIVE_EMAIL_OAUTH_TENANT_ID";
        const CID: &str = "KHIVE_EMAIL_OAUTH_CLIENT_ID";
        const CS: &str = "KHIVE_EMAIL_OAUTH_CLIENT_SECRET";

        std::env::set_var(TID, "fake-tenant");
        std::env::set_var(CID, "fake-client-id");
        std::env::set_var(CS, "fake-secret");

        let result = build_auth();

        std::env::remove_var(TID);
        std::env::remove_var(CID);
        std::env::remove_var(CS);

        let auth = result.expect("all three OAuth vars set must succeed");
        assert!(
            matches!(auth, EmailAuth::OAuth { .. }),
            "expected OAuth variant"
        );
    }

    /// Only tenant_id and client_id present (no secret) → clear error.
    #[test]
    fn build_auth_partial_oauth_returns_error() {
        const TID: &str = "KHIVE_EMAIL_OAUTH_TENANT_ID";
        const CID: &str = "KHIVE_EMAIL_OAUTH_CLIENT_ID";
        const CS: &str = "KHIVE_EMAIL_OAUTH_CLIENT_SECRET";

        std::env::set_var(TID, "fake-tenant");
        std::env::set_var(CID, "fake-client-id");
        std::env::remove_var(CS);
        // Ensure KHIVE_EMAIL_PASSWORD is also absent so we land in the partial
        // branch regardless.
        std::env::remove_var("KHIVE_EMAIL_PASSWORD");

        let result = build_auth();

        std::env::remove_var(TID);
        std::env::remove_var(CID);

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
        const TID: &str = "KHIVE_EMAIL_OAUTH_TENANT_ID";
        const CID: &str = "KHIVE_EMAIL_OAUTH_CLIENT_ID";
        const CS: &str = "KHIVE_EMAIL_OAUTH_CLIENT_SECRET";
        const PW: &str = "KHIVE_EMAIL_PASSWORD";

        std::env::remove_var(TID);
        std::env::remove_var(CID);
        std::env::remove_var(CS);
        std::env::set_var(PW, "test-password");

        let result = build_auth();

        std::env::remove_var(PW);

        let auth = result.expect("no OAuth vars + password must succeed");
        assert!(
            matches!(auth, EmailAuth::Basic { .. }),
            "expected Basic variant"
        );
    }
}
